//! ANSI-aware measurement, truncation and wrapping (port of packages/tui/src/utils.ts).

mod ansi;
mod width;
mod wrap;

pub(crate) use ansi::{AnsiCodeTracker, clean_link_url};
pub use ansi::{
    ansi_len, extract_ansi_code, get_active_background_ansi, link_target, normalize_terminal_output,
    strip_link_targets, strip_terminal_sequences,
};
pub(crate) use width::grapheme_width;
pub use width::{is_cjk_break, is_cjk_punctuation, visible_width};
pub use wrap::{apply_background_to_line, slice_with_width, truncate_to_width, wrap_text_with_ansi};
