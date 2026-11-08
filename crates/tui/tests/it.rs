//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

mod support;

#[path = "attachments.rs"]
mod attachments;
#[path = "autocomplete_parity.rs"]
mod autocomplete_parity;
#[path = "clipboard_copy.rs"]
mod clipboard_copy;
#[path = "clipboard_image.rs"]
mod clipboard_image;
#[path = "editor_autocomplete_async.rs"]
mod editor_autocomplete_async;
#[path = "editor_autocomplete_parity.rs"]
mod editor_autocomplete_parity;
#[path = "editor_component_parity.rs"]
mod editor_component_parity;
#[path = "editor_parity.rs"]
mod editor_parity;
#[path = "escape_timeout.rs"]
mod escape_timeout;
#[path = "fullscreen_clicks.rs"]
mod fullscreen_clicks;
#[path = "fullscreen_scroll.rs"]
mod fullscreen_scroll;
#[path = "highlight.rs"]
mod highlight;
#[path = "keyboard_protocol.rs"]
mod keyboard_protocol;
#[path = "kitty_images.rs"]
mod kitty_images;
#[path = "latex_parity.rs"]
mod latex_parity;
#[path = "markdown_latex_parity.rs"]
mod markdown_latex_parity;
#[path = "markdown_parity.rs"]
mod markdown_parity;
#[path = "marked_parity.rs"]
mod marked_parity;
#[path = "marked_perf.rs"]
mod marked_perf;
#[path = "osc11.rs"]
mod osc11;
#[path = "parity.rs"]
mod parity;
#[path = "paste_parity.rs"]
mod paste_parity;
#[path = "ported.rs"]
mod ported;
#[path = "render_debug.rs"]
mod render_debug;
#[path = "right_click_paste.rs"]
mod right_click_paste;
#[path = "screen.rs"]
mod screen;
#[path = "split_kitty_images.rs"]
mod split_kitty_images;
#[path = "split_view.rs"]
mod split_view;
#[path = "tty_writer.rs"]
mod tty_writer;
#[path = "viewport.rs"]
mod viewport;
#[path = "vim_editor.rs"]
mod vim_editor;
#[path = "vim_parity.rs"]
mod vim_parity;
#[path = "widgets_parity.rs"]
mod widgets_parity;
#[path = "word_segment.rs"]
mod word_segment;
