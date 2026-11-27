//! Claude-Code-style links in the fullscreen viewport: with `link_hover` on, the link under the pointer (an OSC 8
//! hyperlink or a plain-text URL) is underlined while the pointer is over it, pointer motion that changes nothing
//! asks for no paint, and `LinkHover` picks the mouse tracking that hover needs.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tui::{
    ENABLE_BUTTON_MOTION_MOUSE, FullscreenViewport, KeyContext, KeybindingsManager, LinkHover, MouseDispatch,
    MouseEvent, MouseResult, MouseTarget, MouseTargets, Rect, ScrollView, Scrollbar, ViewportInput, hyperlink,
    link_target, osc8_link_at_column, strip_link_targets, truncate_to_width, tui_keybindings, visible_width,
    wrap_text_with_ansi,
};

const COLS: usize = 48;
const ROWS: usize = 4;
/// The transcript's rows; the last screen row is the dock.
const TRANSCRIPT: usize = 3;
const ALL_MOTION: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1006h";

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(tui_keybindings(), Default::default(), KeyContext::default())
}

fn hovering() -> FullscreenViewport {
    let mut view = FullscreenViewport::new(ScrollView::new(true, Scrollbar::Hidden));
    view.link_hover = true;
    view.set_size(COLS, ROWS);
    view
}

/// A frame: `lines` in the transcript's rows, then a dock row.
fn paint(view: &mut FullscreenViewport, lines: &[&str]) -> Vec<String> {
    let rect = Rect {
        x: 0,
        y: 0,
        width: COLS,
        height: TRANSCRIPT,
    };
    let mut screen = view.layout(lines.iter().map(ToString::to_string).collect(), rect);
    screen.push("> dock https://dock.example/x".into());
    view.finish_frame(screen, COLS, ROWS)
}

/// A pointer motion (no button) to 0-based column `x`, row `y`.
fn motion(x: usize, y: usize) -> String {
    format!("\x1b[<35;{};{}M", x + 1, y + 1)
}

fn without_underline(line: &str) -> String {
    line.replace("\x1b[4m", "").replace("\x1b[24m", "")
}

#[test]
fn tui_fullscreen_hover_underlines_the_plain_url_under_the_pointer() {
    let keys = keys();
    let mut view = hovering();
    let row = "PR: https://github.com/a/b/pull/1 is open";
    let lines = [row, "second row"];
    assert_eq!(paint(&mut view, &lines)[0], row);
    let now = Instant::now();
    assert_eq!(view.handle_input(&motion(10, 0), &keys, now), ViewportInput::Consumed);
    let frame = paint(&mut view, &lines);
    assert_eq!(frame[0], "PR: \x1b[4mhttps://github.com/a/b/pull/1\x1b[24m is open");
    assert_eq!(frame[1], "second row");
    // Elsewhere on the same link: nothing to paint.
    assert_eq!(view.handle_input(&motion(30, 0), &keys, now), ViewportInput::Unchanged);
    assert_eq!(paint(&mut view, &lines)[0], frame[0]);
    // Off the link: the underline goes.
    assert_eq!(view.handle_input(&motion(38, 0), &keys, now), ViewportInput::Consumed);
    assert_eq!(paint(&mut view, &lines)[0], row);
    assert_eq!(view.handle_input(&motion(2, 1), &keys, now), ViewportInput::Unchanged);
}

#[test]
fn tui_fullscreen_hover_underlines_an_osc8_link_and_keeps_it_whole() {
    let keys = keys();
    let mut view = hovering();
    let row = "see \x1b]8;;https://example.com/docs\x1b\\the docs\x1b]8;;\x1b\\ here";
    let lines = [row];
    paint(&mut view, &lines);
    let now = Instant::now();
    assert_eq!(view.handle_input(&motion(9, 0), &keys, now), ViewportInput::Consumed);
    assert_eq!(
        paint(&mut view, &lines)[0],
        "see \x1b]8;;https://example.com/docs\x1b\\\x1b[4mthe docs\x1b[24m\x1b]8;;\x1b\\ here"
    );
    assert_eq!(view.handle_input(&motion(4, 0), &keys, now), ViewportInput::Unchanged);
    assert_eq!(view.handle_input(&motion(13, 0), &keys, now), ViewportInput::Consumed);
    assert_eq!(paint(&mut view, &lines)[0], row);
}

/// A reset inside the link does not end its underline, wide graphemes before it keep its columns, and only SGR 4
/// and 24 are added: without them the row is the one the application painted.
#[test]
fn tui_fullscreen_hover_underline_survives_resets_and_wide_text() {
    let keys = keys();
    let now = Instant::now();
    let reset = "x \x1b[34mhttps://a.dev/\x1b[0mpath\x1b[39m y";
    let wide = "日本語 https://x.dev/a end";
    let lines = [reset, wide];
    let mut view = hovering();
    paint(&mut view, &lines);
    view.handle_input(&motion(5, 0), &keys, now);
    view.handle_input(&motion(10, 1), &keys, now);
    let frame = paint(&mut view, &lines);
    assert_eq!(frame[0], reset);
    assert_eq!(frame[1], "日本語 \x1b[4mhttps://x.dev/a\x1b[24m end");
    assert_eq!(visible_width(&frame[1]), visible_width(wide));

    view.handle_input(&motion(18, 0), &keys, now);
    let frame = paint(&mut view, &lines);
    assert_eq!(
        frame[0],
        "x \x1b[34m\x1b[4mhttps://a.dev/\x1b[0m\x1b[4mpath\x1b[24m\x1b[39m y"
    );
    assert_eq!(without_underline(&frame[0]), reset);
    // Inside the wide text, before the URL: no link.
    view.handle_input(&motion(3, 1), &keys, now);
    assert_eq!(paint(&mut view, &lines)[1], wide);
}

/// The underline is found again in each frame: output that moves the link away from a still pointer takes the
/// underline with it, and a key, a lost focus or the pointer leaving the transcript ends it.
#[test]
fn tui_fullscreen_hover_ends_when_the_link_or_the_pointer_leaves() {
    let keys = keys();
    let now = Instant::now();
    let link = "go https://x.dev/a";
    let mut view = hovering();
    paint(&mut view, &[link]);
    view.handle_input(&motion(5, 0), &keys, now);
    assert_ne!(paint(&mut view, &[link])[0], link);
    assert_eq!(paint(&mut view, &["moved", link])[0], "moved");

    let underlined = |view: &mut FullscreenViewport| paint(view, &[link])[0] != link;
    let below = motion(5, TRANSCRIPT);
    for leave in ["a", "\x1b[O", below.as_str()] {
        view.handle_input(&motion(5, 0), &keys, now);
        assert!(underlined(&mut view), "{leave:?}");
        view.handle_input(leave, &keys, now);
        assert!(!underlined(&mut view), "{leave:?}");
    }
    // The dock row's URL is outside the transcript: never underlined.
    view.handle_input(&motion(10, TRANSCRIPT), &keys, now);
    assert_eq!(paint(&mut view, &[link])[TRANSCRIPT], "> dock https://dock.example/x");
}

#[test]
fn tui_fullscreen_hover_is_off_by_default() {
    let keys = keys();
    let mut view = FullscreenViewport::new(ScrollView::new(true, Scrollbar::Hidden));
    view.set_size(COLS, ROWS);
    let link = "go https://x.dev/a";
    paint(&mut view, &[link]);
    assert_eq!(
        view.handle_input(&motion(5, 0), &keys, Instant::now()),
        ViewportInput::Unchanged
    );
    assert_eq!(paint(&mut view, &[link])[0], link);
}

/// An underlined link still opens: a plain click on its OSC 8 link, an Option/Alt click on a plain URL.
#[test]
fn tui_fullscreen_hovered_links_still_open() {
    let keys = keys();
    let opened: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut view = hovering();
    let sink = Arc::clone(&opened);
    view.open_url = Some(Arc::new(move |url: &str| sink.lock().unwrap().push(url.to_string())));
    let lines = [
        "see \x1b]8;;https://example.com/docs\x1b\\the docs\x1b]8;;\x1b\\ here",
        "PR: https://github.com/a/b/pull/1 is open",
    ];
    paint(&mut view, &lines);
    let now = Instant::now();
    view.handle_input(&motion(9, 0), &keys, now);
    paint(&mut view, &lines);
    for data in ["\x1b[<0;10;1M", "\x1b[<0;10;1m"] {
        view.handle_input(data, &keys, now);
    }
    let later = now + std::time::Duration::from_secs(2);
    view.handle_input(&motion(10, 1), &keys, later);
    paint(&mut view, &lines);
    for data in ["\x1b[<8;11;2M", "\x1b[<8;11;2m"] {
        view.handle_input(data, &keys, later);
    }
    assert_eq!(
        *opened.lock().unwrap(),
        ["https://example.com/docs", "https://github.com/a/b/pull/1"]
    );
}

/// `terminal.linkHover`: `On` asks for all-motion tracking inside a multiplexer too, `Auto` hovers only where
/// all-motion is on anyway (outside multiplexers), `Off` never hovers and keeps the tracking.
#[test]
fn tui_link_hover_picks_the_mouse_tracking() {
    let tmux = |name: &str| (name == "TMUX").then(|| "/tmp/tmux-1/default,1,0".to_string());
    let plain = |name: &str| (name == "TERM").then(|| "xterm-256color".to_string());
    assert_eq!(LinkHover::default(), LinkHover::Auto);
    assert_eq!(LinkHover::On.mouse_tracking(&tmux), (ALL_MOTION, true));
    assert_eq!(
        LinkHover::Auto.mouse_tracking(&tmux),
        (ENABLE_BUTTON_MOTION_MOUSE, false)
    );
    assert_eq!(
        LinkHover::Off.mouse_tracking(&tmux),
        (ENABLE_BUTTON_MOTION_MOUSE, false)
    );
    assert_eq!(LinkHover::Auto.mouse_tracking(&plain), (ALL_MOTION, true));
    assert_eq!(LinkHover::On.mouse_tracking(&plain), (ALL_MOTION, true));
    assert_eq!(LinkHover::Off.mouse_tracking(&plain), (ALL_MOTION, false));
    assert_eq!(LinkHover::Auto.mouse_tracking(&plain).0, tui::mouse_tracking(&plain));
}

/// Transcript items that take every mouse event, as the app's message and cell items do.
#[derive(Default)]
struct ItemsTakingTheMouse {
    events: usize,
}

impl MouseTargets for ItemsTakingTheMouse {
    fn dispatch_at(&mut self, _: &MouseEvent) -> Option<MouseDispatch> {
        self.events += 1;
        Some(MouseDispatch {
            result: MouseResult {
                handled: Some(true),
                ..MouseResult::default()
            },
            target: MouseTarget {
                id: 1,
                origin_x: 0,
                origin_y: 0,
                width: COLS,
                height: TRANSCRIPT,
            },
        })
    }

    fn dispatch_to(&mut self, target: &MouseTarget, _: &MouseEvent) -> Option<MouseDispatch> {
        self.events += 1;
        Some(MouseDispatch {
            result: MouseResult::default(),
            target: *target,
        })
    }
}

/// Items under the pointer that take the mouse neither hide the hover underline nor keep an Option-click from opening
/// the link; a plain click on a plain-text URL stays the item's.
#[test]
fn tui_fullscreen_links_inside_items_that_take_the_mouse() {
    let keys = keys();
    let mut view = hovering();
    let opened: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&opened);
    view.open_url = Some(Arc::new(move |url: &str| sink.lock().unwrap().push(url.to_string())));
    let row = "PR: https://github.com/a/b/pull/1 is open";
    let lines = [row, "second row"];
    paint(&mut view, &lines);
    let mut items = ItemsTakingTheMouse::default();
    let now = Instant::now();
    assert_eq!(
        view.handle_input_with(&motion(10, 0), &keys, now, &mut items),
        ViewportInput::Consumed
    );
    assert_eq!(
        paint(&mut view, &lines)[0],
        "PR: \x1b[4mhttps://github.com/a/b/pull/1\x1b[24m is open"
    );
    for data in ["\x1b[<8;11;1M", "\x1b[<8;11;1m"] {
        view.handle_input_with(data, &keys, now, &mut items);
    }
    assert_eq!(*opened.lock().unwrap(), ["https://github.com/a/b/pull/1"]);
    let before = items.events;
    let later = now + Duration::from_secs(2);
    for data in ["\x1b[<0;11;1M", "\x1b[<0;11;1m"] {
        view.handle_input_with(data, &keys, later, &mut items);
    }
    assert!(items.events > before);
    assert_eq!(opened.lock().unwrap().len(), 1);
}

/// A link wrapped onto several rows keeps its OSC 8 target on each of them: the wrapper closes it at every row end
/// and opens it again at the next row's start, so the viewport finds the whole URL on any row.
#[test]
fn tui_wrap_carries_an_osc8_link_onto_every_row() {
    let url = "https://example.com/a/very/long/path/that/wraps/around";
    let rows = wrap_text_with_ansi(&format!("see {}", hyperlink(url, url)), 20);
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(osc8_link_at_column(&rows[0], 0), None);
    for row in &rows[1..] {
        for column in 0..visible_width(row) {
            assert_eq!(
                osc8_link_at_column(row, column).as_deref(),
                Some(url),
                "{row:?} {column}"
            );
        }
        assert!(row.ends_with("\x1b]8;;\x1b\\"), "{row:?}");
    }
}

/// A link target wraps like any OSC 8 link, so the viewport finds the whole URL on each row, and stripping the targets
/// from the wrapped or truncated rows gives exactly the rows of the text without them: every close the wrapper writes
/// for a target is a target's, and the resets other code writes stay.
#[test]
fn tui_wrapped_link_targets_strip_to_the_rows_without_them() {
    let url = "https://example.com/a/very/long/path/that/wraps/around";
    let styled = format!("\x1b[4m\x1b[36m{url}\x1b[39m\x1b[24m");
    let text = format!("see {styled} and more");
    let rows = wrap_text_with_ansi(&format!("see {} and more", link_target(&styled, url)), 20);
    let plain = wrap_text_with_ansi(&text, 20);
    assert_eq!(rows.len(), plain.len());
    for (row, want) in rows.iter().zip(&plain) {
        assert_eq!(strip_link_targets(row), want.as_str());
        if strip_link_targets(row) != row.as_str() {
            assert_eq!(osc8_link_at_column(row, 0).as_deref(), Some(url), "{row:?}");
        }
    }
    assert!(
        rows.iter().filter(|r| osc8_link_at_column(r, 0).is_some()).count() >= 3,
        "{rows:?}"
    );

    let reset = "\x1b[0m\x1b]8;;\x07";
    let row = format!(
        "{}{reset}",
        truncate_to_width(&format!("see {}", link_target(&styled, url)), 12, "", false)
    );
    let want = format!("{}{reset}", truncate_to_width(&text, 12, "", false));
    assert_eq!(strip_link_targets(&row), want);
    assert!(strip_link_targets(&row).ends_with(reset));
}
