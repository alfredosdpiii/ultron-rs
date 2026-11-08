//! System clipboard off the paint path: an [`arboard`] backend (text, and images off Linux or when the Linux tools
//! fail) with `wl-paste`/`xclip` (and PowerShell under WSL) for image reads,
//! driven by a [`ClipboardService`] thread. Copies are fire-and-forget; reads answer on a channel.

use std::sync::mpsc::{self, Receiver};

use crate::service::ServiceThread;

/// A clipboard image (`ClipboardImage`): its bytes and base MIME type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

/// Clipboard access. Implementations may block (X11/Wayland round trips); only the service thread calls them.
pub trait Clipboard: Send {
    fn get_text(&mut self) -> Result<String, String>;
    fn set_text(&mut self, text: &str) -> Result<(), String>;
    /// A clipboard image (`readClipboardImage`): PNG, JPEG, WebP or GIF as read, other types converted to PNG.
    fn get_image(&mut self) -> Result<ClipboardImage, String>;
    /// A user-visible copy (`copyToClipboard`): the OSC 52 sequence to write when one is due, else why it failed.
    fn copy_checked(&mut self, text: &str) -> Result<Option<String>, String> {
        self.set_text(text).map(|()| None)
    }
}

/// The desktop clipboard through arboard; images through `wl-paste` or `xclip` (PowerShell under WSL), else arboard.
pub struct SystemClipboard {
    inner: Option<arboard::Clipboard>,
    /// Whether an image read may fall back to the desktop clipboard (arboard); off for [`SystemClipboard::with_env`]
    /// until [`SystemClipboard::with_native_images`].
    native_images: bool,
    /// The PATH clipboard tools are looked up on (the injected environment's; `None`: no tool runs).
    path: Option<String>,
    /// The environment the copy decisions read (`DISPLAY`, `SSH_CONNECTION`, ...).
    vars: Vec<(String, String)>,
}

impl SystemClipboard {
    /// The process environment's clipboard (tools on its PATH, native image reads on).
    pub fn new() -> Self {
        Self::with_env(std::env::var("PATH").ok(), std::env::vars().collect()).with_native_images()
    }

    /// Let image reads use the desktop clipboard: the only reader on macOS and Windows, the last on Linux.
    pub fn with_native_images(mut self) -> Self {
        self.native_images = true;
        self
    }

    /// A clipboard whose tools come from `path` and whose decisions read `vars`. The desktop clipboard connection
    /// opens on the first text access (an image read only runs the tools).
    pub fn with_env(path: Option<String>, vars: Vec<(String, String)>) -> Self {
        Self {
            inner: None,
            native_images: false,
            path,
            vars,
        }
    }

    fn board(&mut self) -> Result<&mut arboard::Clipboard, String> {
        if self.inner.is_none() {
            self.inner = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
        }
        self.inner.as_mut().ok_or_else(|| "clipboard unavailable".to_string())
    }
}

impl Default for SystemClipboard {
    fn default() -> Self {
        Self::new()
    }
}

/// A fresh `pi-wsl-clip-<pid>-<nanos>-<n>.<extension>` path in the temp dir, for the WSL clipboard bridges (the
/// PowerShell image read, the text written for `clip.exe`).
pub(crate) fn wsl_temp_file(extension: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("pi-wsl-clip-{}-{nanos}-{n}.{extension}", std::process::id()))
}

impl Clipboard for SystemClipboard {
    fn get_text(&mut self) -> Result<String, String> {
        self.board()?.get_text().map_err(|e| e.to_string())
    }

    fn copy_checked(&mut self, text: &str) -> Result<Option<String>, String> {
        let vars = &self.vars;
        let var = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let env = crate::clipboard_copy::CopyEnv {
            platform: jsrt::process::platform(),
            var: &var,
            proc_version: std::fs::read_to_string("/proc/version").ok(),
        };
        let mut io = crate::clipboard_copy::SystemCopyIo::new(self.path.clone());
        crate::clipboard_copy::copy_to_clipboard(text, &env, &mut io)
    }

    fn set_text(&mut self, text: &str) -> Result<(), String> {
        self.board()?.set_text(text.to_string()).map_err(|e| e.to_string())
    }

    /// `readClipboardImage` (crate::clipboard_image): on Linux `wl-paste` in a Wayland session or WSL, then
    /// `xclip`, then PowerShell under WSL, then arboard; elsewhere arboard.
    fn get_image(&mut self) -> Result<ClipboardImage, String> {
        use crate::clipboard_image::{ImageRead, ImageSources};
        let path = self.path.clone();
        let vars = self.vars.clone();
        let var = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let proc_version = std::fs::read_to_string("/proc/version").ok();
        let mut run = |program: &str, args: &[&str]| crate::proc::run(program, path.as_deref(), args, None, None);
        let native_images = self.native_images;
        let mut native = || {
            if !native_images {
                return ImageRead::Failed;
            }
            let Ok(board) = self.board() else {
                return ImageRead::Failed;
            };
            match board.get_image() {
                Ok(image) => match crate::clipboard_image::rgba_to_png(
                    image.width as u32,
                    image.height as u32,
                    image.bytes.into_owned(),
                ) {
                    Some(bytes) if !bytes.is_empty() => ImageRead::Image {
                        bytes,
                        mime_type: "image/png".to_string(),
                    },
                    _ => ImageRead::NoImage,
                },
                Err(arboard::Error::ContentNotAvailable) => ImageRead::NoImage,
                Err(_) => ImageRead::Failed,
            }
        };
        let temp_file = || wsl_temp_file("png");
        let sources = ImageSources {
            platform: jsrt::process::platform(),
            var: &var,
            proc_version: proc_version.as_deref(),
            run: &mut run,
            native: &mut native,
            temp_file: &temp_file,
        };
        match crate::clipboard_image::read_clipboard_image(sources) {
            crate::clipboard_image::ImageRead::Image { bytes, mime_type } => Ok(ClipboardImage { bytes, mime_type }),
            _ => Err("no image on the clipboard".to_string()),
        }
    }
}

/// What a read produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardReply {
    Text(String),
    /// A clipboard image: standard base64 of its bytes and its MIME type (PNG, JPEG, WebP or GIF).
    Image {
        data: String,
        mime_type: String,
    },
    Error(String),
    /// A [`ClipboardService::copy_checked`] finished: the OSC 52 sequence to write when one is due, or why the
    /// copy failed.
    Copied(Result<Option<String>, String>),
}

/// Which read to perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardRead {
    Text,
    Image,
    /// An image when the clipboard holds one, else text.
    ImageOrText,
}

enum Job {
    Copy(String),
    CopyChecked(String),
    Read(ClipboardRead),
}

/// A thread owning the clipboard: `copy` and `request` return immediately.
pub struct ClipboardService {
    thread: ServiceThread<Job>,
    replies: Receiver<ClipboardReply>,
}

impl ClipboardService {
    pub fn spawn<C: Clipboard + 'static>(mut clipboard: C) -> std::io::Result<Self> {
        let (reply_tx, replies) = mpsc::channel();
        let thread = ServiceThread::spawn("clipboard", move |jobs| {
            for job in jobs {
                let reply = match job {
                    Job::Copy(text) => {
                        // A yank's copy has no one to answer.
                        let _ = clipboard.set_text(&text);
                        continue;
                    }
                    Job::CopyChecked(text) => Ok(ClipboardReply::Copied(clipboard.copy_checked(&text))),
                    Job::Read(ClipboardRead::Text) => clipboard.get_text().map(ClipboardReply::Text),
                    Job::Read(ClipboardRead::Image) => clipboard.get_image().map(image_reply),
                    Job::Read(ClipboardRead::ImageOrText) => clipboard
                        .get_image()
                        .map(image_reply)
                        .or_else(|_| clipboard.get_text().map(ClipboardReply::Text)),
                };
                if reply_tx.send(reply.unwrap_or_else(ClipboardReply::Error)).is_err() {
                    break;
                }
            }
        })?;
        Ok(Self { thread, replies })
    }

    /// The desktop clipboard.
    pub fn system() -> std::io::Result<Self> {
        Self::spawn(SystemClipboard::new())
    }

    /// Copy `text` (Vim yanks, copy commands). Never blocks.
    pub fn copy(&self, text: &str) {
        self.thread.send(Job::Copy(text.to_string()));
    }

    /// A user-visible copy (`/copy`, the copy keys): answered by a [`ClipboardReply::Copied`]. Never blocks.
    pub fn copy_checked(&self, text: &str) {
        self.thread.send(Job::CopyChecked(text.to_string()));
    }

    /// Start a read; poll [`ClipboardService::try_reply`] from the event loop.
    pub fn request(&self, read: ClipboardRead) {
        self.thread.send(Job::Read(read));
    }

    /// The next finished read, if any. Never blocks.
    pub fn try_reply(&self) -> Option<ClipboardReply> {
        self.replies.try_recv().ok()
    }

    /// The receiver, for event loops that select on several channels.
    pub fn replies(&self) -> &Receiver<ClipboardReply> {
        &self.replies
    }

    /// A closure for `VimEditor::on_yank`.
    pub fn yank_sink(&self) -> Box<dyn FnMut(&str)> {
        let tx = self.thread.sender();
        Box::new(move |text: &str| tx.send(Job::Copy(text.to_string())))
    }

    /// Finish queued jobs and stop (yank sinks handed out earlier become no-ops).
    pub fn shutdown(self) {
        self.thread.shutdown();
    }
}

fn image_reply(image: ClipboardImage) -> ClipboardReply {
    ClipboardReply::Image {
        data: jsrt::base64::encode(image.bytes),
        mime_type: image.mime_type,
    }
}
