//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "attachments.rs"]
mod attachments;
#[path = "autocomplete_parity.rs"]
mod autocomplete_parity;
#[path = "autocomplete_sources.rs"]
mod autocomplete_sources;
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
#[path = "keybindings_file.rs"]
mod keybindings_file;
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
#[path = "notify.rs"]
mod notify;
#[path = "osc11.rs"]
mod osc11;
#[path = "outline.rs"]
mod outline;
#[path = "p_tui.rs"]
mod p_tui;
#[path = "parity.rs"]
mod parity;
#[path = "paste_parity.rs"]
mod paste_parity;
#[path = "ported.rs"]
mod ported;
#[path = "qol_keys.rs"]
mod qol_keys;
#[path = "render_debug.rs"]
mod render_debug;
#[path = "right_click_paste.rs"]
mod right_click_paste;
#[path = "screen.rs"]
mod screen;
#[path = "split_kitty_images.rs"]
mod split_kitty_images;
#[path = "status_line.rs"]
mod status_line;
#[path = "theme_parity.rs"]
mod theme_parity;
#[path = "theme_registry_parity.rs"]
mod theme_registry_parity;
#[path = "theme_styles_parity.rs"]
mod theme_styles_parity;
#[path = "transcript_search.rs"]
mod transcript_search;
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

#[test]
fn every_test_file_is_built() {
    let (merged, manifest) = (include_str!("it.rs"), include_str!("../Cargo.toml"));
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for entry in std::fs::read_dir(tests).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".rs") && name != "it.rs" {
            let listed = merged.contains(&format!("#[path = \"{name}\"]"))
                || manifest.contains(&format!("path = \"tests/{name}\""));
            assert!(
                listed,
                "tests/{name} is neither a module of tests/it.rs nor a [[test]] of Cargo.toml"
            );
        }
    }
}
