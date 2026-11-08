//! Rust port of packages/tui: ANSI-aware text measurement, key parsing, stdin sequence splitting,
//! basic components and the main-screen differential renderer.

mod attachments;
mod autocomplete;
mod clipboard;
mod clipboard_copy;
mod clipboard_image;
mod components;
mod editing;
mod editor;
mod frame;
mod fullscreen;
mod fuzzy;
mod highlight;
pub mod hljs;
mod image;
mod input;
mod keybindings;
mod keybindings_file;
mod keyboard_protocol;
mod keys;
mod latex;
mod markdown;
mod marked;
mod mouse;
mod overlay;
pub mod proc;
mod render_debug;
mod renderer;
mod screen;
mod scroll_view;
mod select;
mod service;
mod stdin_buffer;
mod tables;
mod terminal;
mod terminal_colors;
mod tty_writer;
mod utils;
mod viewport;
mod vim;
mod vim_editor;
mod word_segment;

pub use attachments::{
    Attachments, BracketedPaste, DraftRecall, EnhancedPaste, PASTE_END, PASTE_START, PasteAction, PasteEvent,
    PasteResult, RAW_PASTE_KEY, chip_band, classify_paste, decode_reencoded_paste_controls, draft_chips,
    extract_image_paste_paths_from_text, extract_paste_paths_from_text, placeholder_spans,
};
pub use autocomplete::{
    AbortSignal, AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CombinedAutocompleteProvider,
    Completion, SlashCommand, SuggestionsJob, apply_completion,
};
pub use clipboard::{Clipboard, ClipboardImage, ClipboardRead, ClipboardReply, ClipboardService, SystemClipboard};
pub use clipboard_copy::{CopyEnv, CopyIo, copy_to_clipboard, platform};
pub use clipboard_image::{
    ImageRead, ImageSources, extension_for_image_mime_type, is_wayland_session, is_wsl, powershell_script,
    read_clipboard_image, rgba_to_png,
};
pub use components::{BgFn, Component, Container, Panel, Spacer, Text, TruncatedText};
pub use editing::{find_word_backward, find_word_forward};
pub use editor::{AUTOCOMPLETE_DEBOUNCE_MS, Editor, word_wrap_line};
pub use frame::CURSOR_MARKER;
pub use fullscreen::{
    ENABLE_BUTTON_MOTION_MOUSE, FullscreenViewport, MouseDispatch, MouseTarget, MouseTargets, UrlOpener, ViewportInput,
    is_focus_report, is_right_click_press, mouse_tracking, osc8_link_at_column, retarget_mouse_event,
    right_click_paste_enabled, word_selection,
};
pub use fuzzy::{fuzzy_filter, fuzzy_match};
pub use highlight::{
    HighlightColors, HighlightStream, highlight_code, highlight_lines, supports_language, warm, warmed,
};
pub use image::{
    Capabilities, CapabilityOverrides, DEFAULT_CELL, DELETE_ALL_KITTY_IMAGES, DELETE_ALL_KITTY_PLACEMENTS, Dimensions,
    HYPERLINKS_ENV, Image, ImageProtocol, KittyImageMetadata, RenderOptions, allocate_image_id,
    calculate_image_cell_size, capabilities, color_mode, crop_kitty_image_line, delete_kitty_image,
    detect_capabilities, encode_iterm2, encode_kitty, hyperlink, image_dimensions, image_fallback,
    kitty_image_metadata, kitty_image_placement, register_kitty_image_metadata, render_image,
};
pub use input::{Input, InputEvent};
pub use keybindings::{KeybindingDefinition, KeybindingsManager, UserBindings, tui_keybindings};
pub use keybindings_file::{keybindings_path, load_user_bindings, migrate_keybindings_config};
pub use keyboard_protocol::{KeyboardProtocol, NegotiationSequence, parse_negotiation_sequence};
pub use keys::{KeyContext, decode_printable_key, is_key_release, matches_key, parse_key};
pub use latex::render_latex;
pub use markdown::{
    DefaultTextStyle, HighlightFn, ListItem, Markdown, MarkdownOptions, MarkdownTheme, Token,
    trim_partial_closing_fences,
};
pub use marked::{lex, lex_work, markdown_tokens};
pub use mouse::{MouseButton, MouseEvent, MouseEventType, MouseResult, SgrMouse, parse_sgr_mouse};
pub use overlay::{
    Anchor, OverlayOptions, SizeValue, composite_line, render_split, resolve_overlay_layout, split_widths,
};
pub use render_debug::{
    LocalStamp, RENDER_DUMP_DIR, RenderDebug, TUI_DEBUG_ENV, TUI_DEBUG_REDRAW_ENV, render_dump_name, write_log_path,
};
pub use renderer::Renderer;
pub use screen::{AltScreen, StackEntry, allocate_stack_sizes, stack_lines};
pub use scroll_view::{Rect, ScrollView, Scrollbar, slice_by_column};
pub use select::{
    Loader, SelectEvent, SelectItem, SelectList, SelectListTheme, SettingItem, SettingsEvent, SettingsList,
    SettingsListTheme, StyleFn,
};
pub use service::{JobSender, Jobs, ServiceThread};
pub use stdin_buffer::{StdinBuffer, StdinEvent};
pub use terminal::{
    ENABLE_VIRTUAL_TERMINAL_INPUT, Terminal, resolve_escape_timeout, size, spawn_input_pump, spawn_stdin_pump,
    vt_input_mode,
};
pub use terminal_colors::{
    OSC11_BACKGROUND_QUERY, Osc11Read, Osc11Reader, is_osc11_background_color_response, parse_osc11_background_color,
};
pub use tty_writer::{MAX_PENDING_OUTPUT_BYTES, MAX_TTY_WRITE_CHUNK_BYTES, TtyHandle, TtyWriter, write_all_chunked};
pub use utils::{
    ansi_len, normalize_terminal_output, slice_with_width, strip_terminal_sequences, truncate_to_width, visible_width,
    wrap_text_with_ansi,
};
pub use viewport::{FramePlan, GateDecision, HistoryBatch, HistoryKind, RenderGate, ViewportRenderer};
pub use vim::{Pos, VimBuffer, VimCommand, VimMode, VimState, last_grapheme_start};
pub use vim_editor::VimEditor;
pub use word_segment::word_segments;
