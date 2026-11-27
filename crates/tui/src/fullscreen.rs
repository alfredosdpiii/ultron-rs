//! The fullscreen transcript viewport (the viewport half of `TuiAltScreen` in packages/tui/src/tui-alt-screen.ts):
//! keyboard scrolling (page, half page, line, top/bottom, previous/next OSC 133 prompt), mouse-wheel scrolling,
//! the auto scrollbar (hover reveal, track jump, thumb drag), the jump-to-end indicator, application-owned text
//! selection (drag, double-click words, triple-click lines, auto-scroll at the edge) with copy-on-select, and the
//! flash stack ("Copied!").
//!
//! The application lays the transcript out with [`FullscreenViewport::layout`] (its rows at a rectangle of the
//! screen), composes the rest of the frame, and finishes it with [`FullscreenViewport::finish_frame`]. Input goes
//! through [`FullscreenViewport::handle_input`] first; a finished selection comes back as [`ViewportInput::Copy`]
//! for the application to copy (through its injected clipboard), which then reports with
//! [`FullscreenViewport::copied`]. Time is passed in: timers are deadlines that [`FullscreenViewport::poll`] expires.

mod links;
mod mouse;
mod selection;

pub use links::{osc8_link_at_column, plain_url_at_column};
pub use selection::word_selection;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::frame::is_image_line;
use crate::keybindings::KeybindingsManager;
use crate::keys::is_key_release;
use crate::mouse::parse_sgr_mouse;
use crate::mouse::{MouseButton, MouseEvent, MouseResult};
use crate::overlay::composite_line;
use crate::scroll_view::{Rect, ScrollView, Scrollbar, paint_scrollbar, scrollbar_geometry, slice_by_column};
use crate::utils::{truncate_to_width, visible_width};

/// Rows a page scroll keeps from the previous page (`PAGE_SCROLL_OVERLAP`).
pub const PAGE_SCROLL_OVERLAP: usize = 4;
pub const ALT_WHEEL_SCROLL_MULTIPLIER: i64 = 5;
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const FLASH_DURATION: Duration = Duration::from_millis(1000);
const COPY_ERROR_FLASH_DURATION: Duration = Duration::from_millis(5000);
const AUTO_SCROLL_INTERVAL: Duration = Duration::from_millis(50);
const OSC133_PROMPT: [&str; 2] = ["\x1b]133;A\x07", "\x1b]133;A\x1b\\"];

/// `ENABLE_BUTTON_MOTION_MOUSE`: clicks, wheel, drags and focus reports (inside terminal multiplexers).
pub const ENABLE_BUTTON_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h";
/// `ENABLE_ALL_MOTION_MOUSE`: also pointer motion without a button (scrollbar hover).
pub const ENABLE_ALL_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1006h";
pub const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
const FOCUS_IN: &str = "\x1b[I";
const FOCUS_OUT: &str = "\x1b[O";

/// The mouse tracking `beforeTerminalStart` turns on: multiplexers (tmux, zellij, screen) lag when every pointer
/// movement is forwarded, so they get button-motion tracking. `var` reads the environment.
pub fn mouse_tracking(var: &dyn Fn(&str) -> Option<String>) -> &'static str {
    let term = var("TERM").unwrap_or_default().to_lowercase();
    if var("TMUX").is_some()
        || var("ZELLIJ").is_some()
        || var("STY").is_some()
        || term.starts_with("tmux")
        || term.starts_with("screen")
    {
        ENABLE_BUTTON_MOTION_MOUSE
    } else {
        ENABLE_ALL_MOTION_MOUSE
    }
}

/// `terminal.linkHover`: whether the fullscreen viewport underlines the link under the pointer, which takes
/// all-motion mouse tracking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkHover {
    /// Where all-motion tracking is on anyway: outside terminal multiplexers.
    #[default]
    Auto,
    /// Everywhere: inside multiplexers too, at the cost of every pointer movement reaching the TUI.
    On,
    Off,
}

impl LinkHover {
    /// The mouse tracking the alternate screen turns on ([`mouse_tracking`], or all-motion for `On`) and whether
    /// links are underlined under the pointer ([`FullscreenViewport::link_hover`]) with it.
    pub fn mouse_tracking(self, var: &dyn Fn(&str) -> Option<String>) -> (&'static str, bool) {
        let tracking = match self {
            Self::On => ENABLE_ALL_MOTION_MOUSE,
            Self::Auto | Self::Off => mouse_tracking(var),
        };
        (tracking, self != Self::Off && tracking == ENABLE_ALL_MOTION_MOUSE)
    }
}

/// Strip the leading OSC 133 zone markers of a row (`OSC133_ZONE_PREFIX`).
pub fn strip_zone_prefix(line: &str) -> &str {
    let mut rest = line;
    loop {
        let Some(after) = rest.strip_prefix("\x1b]133;") else {
            return rest;
        };
        let mut chars = after.chars();
        if !matches!(chars.next(), Some('A'..='C')) {
            return rest;
        }
        let tail = chars.as_str();
        if let Some(t) = tail.strip_prefix('\x07') {
            rest = t;
        } else if let Some(t) = tail.strip_prefix("\x1b\\") {
            rest = t;
        } else {
            return rest;
        }
    }
}

/// Opens a clicked OSC 8 hyperlink (`openUrl`); the application injects it (its launcher comes from the session's
/// PATH only).
pub type UrlOpener = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone, Copy)]
enum ViewportKeyAction {
    ScrollPage(i64),
    ScrollHalf(i64),
    ScrollLine(i64),
    Prompt(i64),
    Top,
    Bottom,
}

const VIEWPORT_BINDINGS: [(&str, ViewportKeyAction); 10] = [
    ("tui.altScreen.pageUp", ViewportKeyAction::ScrollPage(-1)),
    ("tui.altScreen.pageDown", ViewportKeyAction::ScrollPage(1)),
    ("tui.altScreen.halfPageUp", ViewportKeyAction::ScrollHalf(-1)),
    ("tui.altScreen.halfPageDown", ViewportKeyAction::ScrollHalf(1)),
    ("tui.altScreen.lineUp", ViewportKeyAction::ScrollLine(-1)),
    ("tui.altScreen.lineDown", ViewportKeyAction::ScrollLine(1)),
    ("tui.altScreen.previousPrompt", ViewportKeyAction::Prompt(-1)),
    ("tui.altScreen.nextPrompt", ViewportKeyAction::Prompt(1)),
    ("tui.altScreen.top", ViewportKeyAction::Top),
    ("tui.altScreen.bottom", ViewportKeyAction::Bottom),
];

/// `TuiMouseDispatchTarget`: the component that took an event and where its box was on the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseTarget {
    /// The application's name for the component.
    pub id: usize,
    pub origin_x: i64,
    pub origin_y: i64,
    pub width: usize,
    pub height: usize,
}

/// `TuiMouseDispatchResult`: what the component answered, and where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseDispatch {
    pub result: MouseResult,
    pub target: MouseTarget,
}

/// The application's mouse-aware components (`dispatchMouseToLayout`, `dispatchMouseToTarget`).
pub trait MouseTargets {
    /// The first component under `event.screen_x`, `event.screen_y` that handles `event` (with its local
    /// coordinates), if any.
    fn dispatch_at(&mut self, event: &MouseEvent) -> Option<MouseDispatch>;
    /// `event` (already in `target`'s coordinates) to the component that took the press or the capture.
    fn dispatch_to(&mut self, target: &MouseTarget, event: &MouseEvent) -> Option<MouseDispatch>;
}

/// An application without mouse-aware components.
pub struct NoMouseTargets;

impl MouseTargets for NoMouseTargets {
    fn dispatch_at(&mut self, _: &MouseEvent) -> Option<MouseDispatch> {
        None
    }
    fn dispatch_to(&mut self, _: &MouseTarget, _: &MouseEvent) -> Option<MouseDispatch> {
        None
    }
}

/// `decodeMouseButton`.
fn decode_button(button: u32) -> MouseButton {
    match button & 3 {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        _ => MouseButton::None,
    }
}

pub fn retarget_mouse_event(event: &MouseEvent, target: &MouseTarget) -> MouseEvent {
    MouseEvent {
        x: event.screen_x - target.origin_x,
        y: event.screen_y - target.origin_y,
        width: target.width,
        height: target.height,
        ..*event
    }
}

/// What the viewport did with an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewportInput {
    /// Not the viewport's: the application handles it.
    Pass,
    /// Handled (paint again).
    Consumed,
    /// Handled, and the frame is as it was: pointer motion that changed no hover (no paint needed).
    Unchanged,
    /// A selection finished with copy-on-select: copy this text, then report with [`FullscreenViewport::copied`].
    Copy(String),
    /// A right-button press with [`FullscreenViewport::right_click_paste`] on: paste the clipboard's text
    /// ([`right_click_paste_enabled`]).
    RightClickPaste,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Granularity {
    Character,
    Word,
    Line,
}

/// `SelectionPoint`: a content row of the transcript (`scroll`) or a screen row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Point {
    row: usize,
    col: usize,
    scroll: bool,
    /// Between cells rather than on one.
    boundary: bool,
}

#[derive(Clone, Copy, Debug)]
struct Range {
    start: Point,
    end: Point,
}

#[derive(Clone, Copy, Debug)]
struct Click {
    at: Instant,
    count: u32,
    row: usize,
    scroll: bool,
    word_start: usize,
    word_end: usize,
}

/// A selection bound in screen coordinates (rows may lie above the screen).
#[derive(Clone, Copy, Debug)]
struct ScreenPoint {
    row: i64,
    col: usize,
    boundary: bool,
}

/// The parsed parts of an SGR mouse report (0-based cell).
#[derive(Clone, Copy, Debug)]
struct Sgr {
    button: u32,
    x: i64,
    y: i64,
    release: bool,
}

struct Flash {
    message: String,
    until: Instant,
}

/// The transcript viewport of a fullscreen frame.
pub struct FullscreenViewport {
    pub view: ScrollView,
    /// The transcript's rectangle and rows in the last frame (`currentLayout`).
    rect: Option<Rect>,
    content: Vec<String>,
    /// The last frame's rows (`previousScreen`).
    screen: Vec<String>,
    cols: usize,
    rows: usize,
    anchor: Option<Point>,
    focus: Option<Point>,
    granularity: Granularity,
    initial: Option<Range>,
    last_click: Option<Click>,
    drag_pointer: Option<(i64, i64)>,
    auto_direction: i64,
    auto_next: Option<Instant>,
    press_active: bool,
    dragged: bool,
    scrollbar_drag: Option<i64>,
    scrollbar_hover: bool,
    indicator_rect: Option<(usize, usize, usize)>,
    flashes: Vec<Flash>,
    /// Rows moved per wheel notch (`wheelScrollLines`).
    pub wheel_lines: i64,
    /// Copy a finished selection (`copyOnSelect`, default on).
    pub copy_on_select: bool,
    /// The jump-to-end label (`scrollToEndIndicator`); `None` shows none.
    pub indicator: Option<String>,
    /// Paint rows with their OSC 133 zone marks (TypeScript strips them; they are invisible either way).
    pub keep_zone_marks: bool,
    /// A right-button press pastes instead of selecting ([`right_click_paste_enabled`];
    /// off by default).
    pub right_click_paste: bool,
    /// Opens an OSC 8 hyperlink clicked with the primary button (`openUrl`); `None` opens nothing.
    pub open_url: Option<UrlOpener>,
    /// The hyperlink under the press, opened when the release ends a click (`pressedUrl`).
    pressed_url: Option<String>,
    /// Underline the link under the pointer, an OSC 8 hyperlink or a plain-text URL, while it is there (pointer
    /// motion needs all-motion tracking: [`LinkHover::mouse_tracking`]); off by default.
    pub link_hover: bool,
    /// The transcript cell (row, column) under the pointer while link hover is on. The link there is found again in
    /// every finished frame, so output or a scroll under a still pointer never leaves a stale underline.
    pointer: Option<(usize, usize)>,
    /// `mouseCapture`, `mousePressTarget`, `mousePressPoint`, `mousePressMoved`, `lastComponentClick`.
    mouse_capture: Option<MouseTarget>,
    press_target: Option<MouseTarget>,
    press_point: Option<(i64, i64)>,
    press_moved: bool,
    last_component_click: Option<(Instant, u32, usize, i64, i64)>,
}

impl Default for FullscreenViewport {
    fn default() -> Self {
        Self::new(ScrollView::new(true, Scrollbar::Auto))
    }
}

impl FullscreenViewport {
    /// A viewport over `view` (the client TUI's transcript: follow-end, auto scrollbar).
    pub fn new(view: ScrollView) -> Self {
        Self {
            view,
            rect: None,
            content: Vec::new(),
            screen: Vec::new(),
            cols: 0,
            rows: 0,
            anchor: None,
            focus: None,
            granularity: Granularity::Character,
            initial: None,
            last_click: None,
            drag_pointer: None,
            auto_direction: 0,
            auto_next: None,
            press_active: false,
            dragged: false,
            scrollbar_drag: None,
            scrollbar_hover: false,
            indicator_rect: None,
            flashes: Vec::new(),
            wheel_lines: 1,
            copy_on_select: true,
            indicator: None,
            keep_zone_marks: false,
            right_click_paste: false,
            open_url: None,
            pressed_url: None,
            link_hover: false,
            pointer: None,
            mouse_capture: None,
            press_target: None,
            press_point: None,
            press_moved: false,
            last_component_click: None,
        }
    }

    /// The terminal size input is measured against (`terminal.columns`, `terminal.rows`).
    pub fn set_size(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
    }

    pub fn viewport_top(&self) -> usize {
        self.view.scroll_top()
    }

    pub fn is_following_output(&self) -> bool {
        self.view.is_following_end()
    }

    /// Forget the layout and gestures (`beforeTerminalStart`: a new alternate screen).
    pub fn reset(&mut self) {
        self.stop_auto_scroll();
        self.press_active = false;
        self.set_hover(false);
        self.scrollbar_drag = None;
        self.flashes.clear();
        self.anchor = None;
        self.focus = None;
        self.granularity = Granularity::Character;
        self.initial = None;
        self.last_click = None;
        self.pressed_url = None;
        self.pointer = None;
        self.dragged = false;
        self.rect = None;
        self.screen.clear();
        self.clear_component_gesture();
        self.mouse_capture = None;
    }

    /// The content of the last [`FullscreenViewport::layout`], to be filled again and passed to the next one: its
    /// strings keep their allocations, so laying out a long transcript every frame copies bytes and allocates nothing.
    pub fn take_content(&mut self) -> Vec<String> {
        std::mem::take(&mut self.content)
    }

    /// Lay the transcript out: `content` (rendered at `view.content_width(rect.width)`) shown in `rect`, scrolled
    /// to the view's position. The rect's rows of the frame, with the scrollbar painted.
    pub fn layout(&mut self, content: Vec<String>, rect: Rect) -> Vec<String> {
        self.view.update_layout(content.len(), rect.height);
        let top = self.view.scroll_top();
        let mut rows: Vec<String> = (0..rect.height)
            .map(|i| {
                content.get(top + i).map_or_else(String::new, |l| {
                    if self.keep_zone_marks {
                        l.clone()
                    } else {
                        strip_zone_prefix(l).to_string()
                    }
                })
            })
            .collect();
        let local = Rect { x: 0, y: 0, ..rect };
        paint_scrollbar(&self.view, local, content.len(), &mut rows, rect.width);
        self.content = content;
        self.rect = Some(rect);
        rows
    }

    /// The layout is not shown (an overlay owns the frame): mouse input then has no transcript to hit.
    pub fn hide_layout(&mut self) {
        self.rect = None;
        self.pointer = None;
    }

    /// Finish a `width` x `height` frame (`doRender` after the layout): the jump-to-end indicator, the selection
    /// highlight, the flashes and the underline of the link under the pointer.
    pub fn finish_frame(&mut self, screen: Vec<String>, width: usize, height: usize) -> Vec<String> {
        self.cols = width;
        self.rows = height;
        let mut screen = self.composite_indicator(screen, width);
        if screen.len() > height {
            screen.drain(..screen.len() - height);
        }
        let screen = self.apply_selection(screen);
        let mut screen = self.composite_flashes(screen, width, height);
        self.underline_hovered_link(&mut screen);
        self.screen = screen
            .iter()
            .map(|l| {
                if is_image_line(l) || visible_width(l) <= width {
                    l.clone()
                } else {
                    slice_by_column(l, 0, width)
                }
            })
            .collect();
        screen
    }

    /// `flash`: a transient message at the top right.
    pub fn flash(&mut self, message: &str, duration: Duration, now: Instant) {
        self.flashes.push(Flash {
            message: message.to_string(),
            until: now + duration,
        });
    }

    /// The application copied a selection: "Copied!", or the error for five seconds.
    pub fn copied(&mut self, result: Result<(), String>, now: Instant) {
        match result {
            Ok(()) => self.flash("Copied!", FLASH_DURATION, now),
            Err(e) => {
                let message = if e.is_empty() { "Copy failed".to_string() } else { e };
                self.flash(&message, COPY_ERROR_FLASH_DURATION, now);
            }
        }
    }

    /// Timers due by `now`: the scrollbar hides, flashes expire, a held drag auto-scrolls. True when the frame changed.
    pub fn poll(&mut self, now: Instant) -> bool {
        let mut changed = self.view.poll(now);
        let before = self.flashes.len();
        self.flashes.retain(|f| f.until > now);
        changed |= self.flashes.len() != before;
        if self.auto_next.is_some_and(|at| at <= now) {
            self.auto_next = Some(now + AUTO_SCROLL_INTERVAL);
            changed |= self.auto_scroll_tick();
        }
        changed
    }

    /// The next instant [`Self::poll`] has work.
    pub fn deadline(&self) -> Option<Instant> {
        [
            self.view.deadline(),
            self.auto_next,
            self.flashes.iter().map(|f| f.until).min(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Whether a non-empty selection is active (`hasActiveSelection`).
    pub fn has_active_selection(&self) -> bool {
        self.selection_text().is_some()
    }

    /// Route `data` (`handleViewportInput`): focus reports, wheel, mouse, then the viewport keys.
    pub fn handle_input(&mut self, data: &str, keys: &KeybindingsManager, now: Instant) -> ViewportInput {
        self.handle_input_with(data, keys, now, &mut NoMouseTargets)
    }

    /// [`Self::handle_input`] with the application's mouse-aware components: a press on one becomes its gesture
    /// (its drag and release go to it, and a release on the press cell adds a click), and a click that ends an
    /// empty text selection goes to the component under it (`handleMouseEvent`).
    pub fn handle_input_with(
        &mut self,
        data: &str,
        keys: &KeybindingsManager,
        now: Instant,
        targets: &mut dyn MouseTargets,
    ) -> ViewportInput {
        if !is_mouse_sequence(data) {
            self.pointer = None;
        }
        if data == FOCUS_OUT {
            return self.handle_focus_out();
        }
        if data == FOCUS_IN || is_mouse_sequence(data) && parse_sgr_mouse(data).is_none() && parse_wheel(data).is_none()
        {
            return ViewportInput::Consumed;
        }
        if let Some((direction, x, y, button)) = parse_wheel(data) {
            return self.handle_wheel(direction, x, y, button, targets);
        }
        if let Some(m) = parse_sgr_mouse(data) {
            return self.handle_mouse(
                Sgr {
                    button: m.button,
                    x: m.col,
                    y: m.row,
                    release: m.release,
                },
                now,
                targets,
            );
        }
        self.handle_viewport_key(data, keys)
    }

    fn apply_viewport_action(&mut self, action: ViewportKeyAction) {
        let height = self.view.viewport_height();
        let page = height.saturating_sub(PAGE_SCROLL_OVERLAP).max(1) as i64;
        let half = (height / 2).max(1) as i64;
        match action {
            ViewportKeyAction::ScrollPage(dir) => self.scroll(dir * page),
            ViewportKeyAction::ScrollHalf(dir) => self.scroll(dir * half),
            ViewportKeyAction::ScrollLine(dir) => self.scroll(dir),
            ViewportKeyAction::Prompt(dir) => self.scroll_to_prompt(dir),
            ViewportKeyAction::Top => self.view.scroll_to_start(),
            ViewportKeyAction::Bottom => self.view.scroll_to_end(),
        }
    }

    fn handle_viewport_key(&mut self, data: &str, keys: &KeybindingsManager) -> ViewportInput {
        let release = is_key_release(data);
        for (binding, action) in VIEWPORT_BINDINGS {
            if keys.matches(data, binding) {
                if !release {
                    self.apply_viewport_action(action);
                }
                return ViewportInput::Consumed;
            }
        }
        ViewportInput::Pass
    }

    fn scroll(&mut self, lines: i64) {
        self.view.scroll_by(lines);
    }

    /// `scrollToPrompt`: the nearest OSC 133 prompt start above (-1) or below (1) the viewport top.
    fn scroll_to_prompt(&mut self, direction: i64) {
        if self.rect.is_none() {
            return;
        }
        let mut row = self.view.scroll_top() as i64 + direction;
        while row >= 0 && (row as usize) < self.content.len() {
            let line = &self.content[row as usize];
            if OSC133_PROMPT.iter().any(|p| line.starts_with(p)) {
                self.view.scroll_to(row, false);
                return;
            }
            row += direction;
        }
    }

    /// `compositeScrollToEndIndicator`.
    fn composite_indicator(&mut self, mut screen: Vec<String>, width: usize) -> Vec<String> {
        self.indicator_rect = None;
        let (Some(label), Some(rect)) = (self.indicator.as_deref(), self.rect) else {
            return screen;
        };
        if !self.view.follows_end() || self.view.is_following_end() || rect.width == 0 || rect.height == 0 {
            return screen;
        }
        let row = rect.y + rect.height - 1;
        if row >= screen.len() || is_image_line(&screen[row]) {
            return screen;
        }
        let scrollbar = scrollbar_geometry(&self.view, rect, self.content.len(), false).map(|g| g.column);
        let label = truncate_to_width(label, rect.width, "", false);
        let column = rect.x + (rect.width - visible_width(&label)) / 2;
        let right = scrollbar.unwrap_or(rect.x + rect.width);
        let text = truncate_to_width(&label, right.saturating_sub(column), "", false);
        let text_width = visible_width(&text);
        if text_width == 0 {
            return screen;
        }
        screen[row] = composite_line(&screen[row], &text, column, text_width, width);
        self.indicator_rect = Some((row, column, text_width));
        screen
    }

    /// The link under the pointer, found in the finished rows of this frame, underlined.
    fn underline_hovered_link(&self, screen: &mut [String]) {
        let Some((row, col)) = self.pointer else {
            return;
        };
        if !self.rect.is_some_and(|r| r.contains(col as i64, row as i64)) {
            return;
        }
        let Some(line) = screen.get_mut(row).filter(|l| !is_image_line(l)) else {
            return;
        };
        if let Some(link) = links::link_at(line, col, true) {
            *line = links::underline_columns(line, link.start, link.end);
        }
    }

    /// `compositeFlashes`: the newest `height` flashes at the top right.
    fn composite_flashes(&self, mut screen: Vec<String>, width: usize, height: usize) -> Vec<String> {
        let skip = self.flashes.len().saturating_sub(height);
        let lines: Vec<String> = self.flashes[skip..]
            .iter()
            .map(|f| {
                format!(
                    "\x1b[7m{}\x1b[27m",
                    truncate_to_width(&format!(" {} ", f.message), width, "", false)
                )
            })
            .collect();
        if lines.is_empty() {
            return screen;
        }
        if screen.len() < height {
            screen.resize(height, String::new());
        }
        for (row, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            if w == 0 {
                continue;
            }
            screen[row] = composite_line(&screen[row], line, width - w, w, width);
        }
        screen
    }
}

/// `parseWheelEvent`: (direction, x, y, button) of a vertical wheel report (SGR or X10).
fn parse_wheel(data: &str) -> Option<(i64, i64, i64, u32)> {
    if let Some(m) = parse_sgr_mouse(data) {
        if m.button & 64 == 0 {
            return None;
        }
        return match m.button & 3 {
            0 => Some((-1, m.col, m.row, m.button)),
            1 => Some((1, m.col, m.row, m.button)),
            _ => None,
        };
    }
    let chars: Vec<char> = data.chars().collect();
    if chars.len() == 6 && data.starts_with("\x1b[M") {
        let button = (chars[3] as u32).wrapping_sub(32);
        if button & 64 == 0 {
            return None;
        }
        let direction = match button & 3 {
            0 => -1,
            1 => 1,
            _ => return None,
        };
        return Some((direction, chars[4] as i64 - 33, chars[5] as i64 - 33, button));
    }
    None
}

pub fn is_mouse_sequence(data: &str) -> bool {
    parse_sgr_mouse(data).is_some() || (data.chars().count() == 6 && data.starts_with("\x1b[M"))
}

/// Whether `data` is a terminal focus report (`\x1b[I` / `\x1b[O`).
pub fn is_focus_report(data: &str) -> bool {
    data == FOCUS_IN || data == FOCUS_OUT
}

/// Whether right-click paste is on (`process.platform === "win32"` and `TERM_PROGRAM` not `vscode`); `platform` is
/// the Node platform name ([`jsrt::process::platform`]) and `var` reads the environment.
pub fn right_click_paste_enabled(platform: &str, var: &dyn Fn(&str) -> Option<String>) -> bool {
    platform == "win32" && var("TERM_PROGRAM").is_none_or(|program| program.to_lowercase() != "vscode")
}

/// An SGR mouse report that pastes: a press (not a release) of exactly button 2, so no modifier, motion or wheel
/// bits.
pub fn is_right_click_press(button: u32, release: bool) -> bool {
    !release && button == 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_prefix_strips_every_leading_marker() {
        assert_eq!(strip_zone_prefix("\x1b]133;A\x07\x1b]133;B\x1b\\x"), "x");
        assert_eq!(strip_zone_prefix("\x1b]133;D\x07x"), "\x1b]133;D\x07x");
        assert_eq!(strip_zone_prefix("plain"), "plain");
    }
}
