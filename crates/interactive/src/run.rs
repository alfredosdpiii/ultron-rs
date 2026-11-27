//! The interactive TUI's event loop: one thread owns the [`App`] and the terminal. Key input (read on its own
//! thread) and lane events arrive on one channel; each batch of events is applied and then one frame is painted on
//! the alternate screen. Nothing here waits on the network or the disk.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use tui::AltScreen;
use tui::KeyboardProtocol;
use tui::Terminal;
use tui::TtyWriter;
use tui::{GateDecision, RenderGate, ViewportRenderer};

use crate::app::App;
use crate::lane::LaneEvent;

pub const TUI_WRITE_LOG_ENV: &str = "PI_TUI_WRITE_LOG";

/// What the UI thread receives.
pub enum UiEvent {
    Input(String),
    Lane(LaneEvent),
    InputClosed,
    /// Something finished off the UI thread (an autocomplete answer): poll and paint.
    Wake,
}

/// Spinner frame interval (`Loader`).
const ANIMATION: Duration = Duration::from_millis(80);
/// How often an idle loop looks at the terminal size.
const IDLE: Duration = Duration::from_millis(100);
/// The editor's debounce for `@` and trigger-character completion.
const DEBOUNCE: Duration = Duration::from_millis(tui::AUTOCOMPLETE_DEBOUNCE_MS);

/// How long exit waits for the writer thread to drain.
const EXIT_FLUSH: Duration = Duration::from_secs(1);

/// Stdin that stops reading while `paused` (an external editor owns the terminal): it polls before each read, so
/// no keystroke meant for the editor is taken.
struct GatedStdin {
    paused: Arc<AtomicBool>,
}

impl std::io::Read for GatedStdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let stdin = std::io::stdin();
        loop {
            if self.paused.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: 50_000_000,
            };
            match poll(&mut fds, Some(&timeout)) {
                Ok(0) => continue,
                Ok(_) => {
                    if self.paused.load(Ordering::SeqCst) {
                        continue;
                    }
                    return std::io::Read::read(&mut stdin.lock(), buf);
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Forward stdin input sequences into `tx`, not reading while `paused`.
pub fn spawn_gated_input(tx: Sender<UiEvent>, paused: Arc<AtomicBool>) {
    let input = tui::spawn_input_pump(GatedStdin { paused });
    std::thread::spawn(move || {
        for data in input {
            if tx.send(UiEvent::Input(data)).is_err() {
                return;
            }
        }
        let _ = tx.send(UiEvent::InputClosed);
    });
}

struct StoppableStdin {
    stop: Arc<AtomicBool>,
}

impl std::io::Read for StoppableStdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let stdin = std::io::stdin();
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: 50_000_000,
            };
            match poll(&mut fds, Some(&timeout)) {
                Ok(0) => continue,
                Ok(_) if self.stop.load(Ordering::SeqCst) => return Ok(0),
                Ok(_) => return std::io::Read::read(&mut stdin.lock(), buf),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Forward stdin input sequences into `tx` until `stop` becomes true (at most one 50ms poll interval).
pub fn spawn_stoppable_input(tx: Sender<UiEvent>, stop: Arc<AtomicBool>) {
    let input = tui::spawn_input_pump(StoppableStdin { stop });
    std::thread::spawn(move || {
        for data in input {
            if tx.send(UiEvent::Input(data)).is_err() {
                return;
            }
        }
        let _ = tx.send(UiEvent::InputClosed);
    });
}

/// Run `command` on a temporary `prompt.md` holding `content` with the terminal handed over (`editInExternalEditor`):
/// the edited text without a trailing newline, or `None` when the editor failed.
pub fn edit_externally(command: &str, content: &str) -> Option<String> {
    let dir = jsrt::fs::temp_dir().join(format!(
        "pi-editor-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("prompt.md");
    let result = (|| {
        std::fs::write(&path, content).ok()?;
        let mut parts = command.split(' ');
        let editor = parts.next()?;
        let mut out = std::io::stdout();
        let _ = write!(
            out,
            "Launching external editor: {command}\nPi will resume when the editor exits.\n"
        );
        let _ = out.flush();
        let status = std::process::Command::new(editor)
            .args(parts)
            .arg(&path)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        Some(text.strip_suffix('\n').unwrap_or(text).to_string())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Pi's Ctrl+Z (`suspendTui`): stop this process with SIGTSTP; the shell's `fg` (SIGCONT) continues it and the call
/// returns. Only the product's run loop installs it ([`run_stdout`]); tests inject their own hook.
fn suspend_process() -> Result<(), String> {
    if cfg!(windows) {
        return Err("Suspend to background is not supported on Windows".into());
    }
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TSTP).map_err(|e| e.to_string())
}

/// The terminal environment's debug outputs (`PI_TUI_WRITE_LOG`, `PI_TUI_DEBUG_REDRAW=1` under `log_dir`, the
/// agent directory, `PI_TUI_DEBUG=1` under `dump_dir`) and whether this is Termux (`TERMUX_VERSION` non-empty).
/// `local` is the local time a write-log directory names its file by; `clock` stamps redraw lines and dumps.
pub fn terminal_debug(
    var: &dyn Fn(&str) -> Option<String>,
    log_dir: &std::path::Path,
    dump_dir: &std::path::Path,
    local: tui::LocalStamp,
    pid: u32,
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
) -> (Option<Arc<tui::RenderDebug>>, bool) {
    let write_log = var(TUI_WRITE_LOG_ENV).and_then(|value| {
        let is_dir = std::fs::metadata(&value).is_ok_and(|m| m.is_dir());
        tui::write_log_path(&value, is_dir, local, pid)
    });
    let debug = tui::RenderDebug::from_env(var, write_log, log_dir, dump_dir, clock);
    let termux = var("TERMUX_VERSION").is_some_and(|v| !v.is_empty());
    (debug.map(Arc::new), termux)
}

/// The product entry point on the process terminal, until the app exits (`size` reports the terminal's columns and
/// rows): starts the background services of the enabled additions ([`crate::qol::install_services`]) and paints
/// through the writer thread when `qol.tty_writer` is on.
pub fn run_stdout(app: &mut App, events: &Receiver<UiEvent>, size: &dyn Fn() -> (u16, u16)) -> std::io::Result<()> {
    crate::qol::install_services(app, std::env::var("PATH").ok())?;
    app.set_suspend_hook(Box::new(suspend_process));
    let (mouse, link_hover) = app.qol().link_hover.mouse_tracking(&|name| std::env::var(name).ok());
    app.set_mouse_tracking(Some(mouse));
    app.set_link_hover(link_hover);
    app.set_right_click_paste(tui::right_click_paste_enabled(jsrt::process::platform(), &|name| {
        std::env::var(name).ok()
    }));
    let mut terminal = Terminal::new(std::io::stdout());
    let tty = if app.qol().tty_writer {
        Some(TtyWriter::stdout()?)
    } else {
        None
    };
    run_on(app, events, &mut terminal, size, tty.as_ref())
}

/// Frames are written through `tty` (the terminal writer thread) instead of `terminal` when given; raw mode and
/// bracketed paste still go through `terminal`.
fn run_on<W: Write>(
    app: &mut App,
    events: &Receiver<UiEvent>,
    terminal: &mut Terminal<W>,
    size: &dyn Fn() -> (u16, u16),
    tty: Option<&TtyWriter>,
) -> std::io::Result<()> {
    terminal.start(true)?;
    terminal.start_keyboard_protocol()?;
    match tty {
        None => {
            let mut write = |data: &str| {
                terminal.out.write_all(data.as_bytes())?;
                terminal.out.flush()
            };
            paint_loop_with(app, events, &mut write, size, None, &mut terminal.keyboard)?;
        }
        Some(tty) => {
            let result = paint_loop_with(
                app,
                events,
                &mut |data| {
                    tty.write(data);
                    if tty.dead() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "terminal writer died",
                        ));
                    }
                    Ok(())
                },
                size,
                Some(&|| tty.pending()),
                &mut terminal.keyboard,
            );
            tty.flush_sync(EXIT_FLUSH);
            result?;
        }
    }
    terminal.stop()
}

/// The event and paint loop between terminal start and stop: every byte for the terminal goes through `out`.
/// `pending` reports the writer's backlog (frames are deferred while it is too deep, with `qol.render_gate`).
pub fn paint_loop(
    app: &mut App,
    events: &Receiver<UiEvent>,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
    size: &dyn Fn() -> (u16, u16),
    pending: Option<&dyn Fn() -> usize>,
) -> std::io::Result<()> {
    paint_loop_with(app, events, out, size, pending, &mut KeyboardProtocol::new())
}

struct PaintState {
    split: bool,
    gated: bool,
    screen: AltScreen,
    viewport: ViewportRenderer,
    gate: RenderGate,
    mouse: Option<&'static str>,
    release: String,
    last_size: (u16, u16),
    dirty: bool,
    debug: Option<Arc<tui::RenderDebug>>,
}

impl PaintState {
    fn new(app: &App) -> Self {
        let split = app.qol().viewport_split;
        let mouse = app.mouse_tracking().filter(|_| !split);
        let release = if mouse.is_some() {
            AltScreen::release_mouse()
        } else {
            String::new()
        };
        Self {
            split,
            gated: app.qol().render_gate,
            screen: AltScreen::new(),
            viewport: ViewportRenderer::new(),
            gate: RenderGate::new(),
            mouse,
            release,
            last_size: (0, 0),
            dirty: true,
            debug: app.render_debug(),
        }
    }

    fn enter(&mut self, app: &mut App) -> String {
        app.reset_viewport();
        self.screen.enter_with_mouse(self.mouse.unwrap_or(""))
    }
}

/// [`paint_loop`] reading the keyboard protocol replies of `keyboard` (queried at terminal start) out of the input:
/// they set the app's Kitty state or the modifyOtherKeys fallback and never reach the components; leaving the
/// terminal for an external editor or a suspend pops the protocol and queries it again on return. The link targets
/// the transcript carries for the fullscreen viewport ([`crate::render_settings::RenderSettings::link_targets`]) are
/// removed from every write here, the one place all bytes leave for the terminal, so it gets what it got without
/// them.
pub fn paint_loop_with(
    app: &mut App,
    events: &Receiver<UiEvent>,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
    size: &dyn Fn() -> (u16, u16),
    pending: Option<&dyn Fn() -> usize>,
    keyboard: &mut KeyboardProtocol,
) -> std::io::Result<()> {
    let debug = app.render_debug();
    let mut logged = |data: &str| {
        let data = tui::strip_link_targets(data);
        if let Some(debug) = &debug {
            debug.wrote(&data);
        }
        out(&data)
    };
    let out: &mut dyn FnMut(&str) -> std::io::Result<()> = &mut logged;
    let mut state = PaintState::new(app);
    if !state.split {
        let bytes = state.enter(app);
        out(&bytes)?;
    }
    loop {
        poll_loop_sources(app, &mut state, keyboard, size);
        handle_external_actions(app, &mut state, keyboard, out)?;
        let deferred = maybe_paint_frame(app, &mut state, pending, out)?;
        if app.exit {
            break;
        }
        let wait = compute_wait(app, &state, keyboard, deferred);
        if poll_events(app, events, keyboard, out, wait)? {
            state.dirty = true;
        }
    }
    finish_paint_loop(app, &mut state, out)
}

fn poll_loop_sources(
    app: &mut App,
    state: &mut PaintState,
    keyboard: &mut KeyboardProtocol,
    size: &dyn Fn() -> (u16, u16),
) {
    let (cols, rows) = size();
    if (cols, rows) != state.last_size {
        state.last_size = (cols, rows);
        state.dirty = true;
    }
    state.dirty |= app.poll_services();
    state.dirty |= app.poll_autocomplete();
    state.dirty |= app.poll_rlm();
    if let Some(fragment) = keyboard.poll(Instant::now()) {
        app.handle_input(&fragment);
        state.dirty = true;
    }
    if !state.split && app.poll_viewport(Instant::now()) {
        state.dirty = true;
    }
}

fn with_terminal_suspended<T>(
    app: &mut App,
    state: &PaintState,
    keyboard: &mut KeyboardProtocol,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
    action: impl FnOnce(&mut App) -> T,
) -> std::io::Result<T> {
    let gate = app.input_gate();
    gate.store(true, Ordering::SeqCst);
    if !state.release.is_empty() {
        out(&state.release)?;
    }
    out(leave_bytes(app, state.split))?;
    leave_keyboard(app, keyboard, out)?;
    let raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    if raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    let result = action(app);
    if raw {
        let _ = crossterm::terminal::enable_raw_mode();
    }
    out("\x1b[?2004h")?;
    out(keyboard.query())?;
    gate.store(false, Ordering::SeqCst);
    app.reset_display = true;
    Ok(result)
}

fn handle_external_actions(
    app: &mut App,
    state: &mut PaintState,
    keyboard: &mut KeyboardProtocol,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if let Some((command, content)) = app.take_edit_request() {
        let edited = with_terminal_suspended(app, state, keyboard, out, |_| edit_externally(&command, &content))?;
        app.finish_edit(edited);
    }
    if app.take_suspend_request() && app.can_suspend() {
        with_terminal_suspended(app, state, keyboard, out, |app| {
            app.suspend();
        })?;
    }
    if app.take_reset_display() {
        reset_display(app, state, out)?;
    }
    Ok(())
}

fn reset_display(
    app: &mut App,
    state: &mut PaintState,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let images = if state.split {
        state.viewport.delete_images()
    } else {
        String::new()
    };
    out(&format!("{images}\x1b[2J\x1b[3J\x1b[H"))?;
    state.screen = AltScreen::new();
    state.viewport = ViewportRenderer::new();
    if !state.split {
        let bytes = state.enter(app);
        out(&bytes)?;
    }
    state.dirty = true;
    Ok(())
}

fn maybe_paint_frame(
    app: &mut App,
    state: &mut PaintState,
    pending: Option<&dyn Fn() -> usize>,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<Option<Duration>> {
    if !state.dirty {
        return Ok(None);
    }
    if state.gated {
        state.gate.request();
        match state.gate.poll(Instant::now(), pending.map(|p| p())) {
            GateDecision::Render => {}
            GateDecision::Wait(d) => return Ok(Some(d)),
            GateDecision::Idle => return Ok(None),
        }
    }
    let started = Instant::now();
    paint_frame(app, state, out)?;
    state.dirty = false;
    state.gate.rendered(started.elapsed());
    Ok(None)
}

fn paint_frame(
    app: &mut App,
    state: &mut PaintState,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    for write in app.terminal_writes.drain(..) {
        out(&write)?;
    }
    let (cols, rows) = (state.last_size.0 as usize, state.last_size.1 as usize);
    state.screen.show_hardware_cursor = app.show_hardware_cursor;
    state.screen.image_protocol = app.image_protocol;
    state.screen.wezterm = app.wezterm;
    state.viewport.show_hardware_cursor = app.show_hardware_cursor;
    state.viewport.clear_on_shrink = app.clear_on_shrink;
    state.viewport.termux = app.termux();
    state.viewport.trace = state.debug.as_ref().is_some_and(|d| d.traces_renders());
    if state.split {
        let plan = app.frame_plan(cols, rows);
        out(&state.viewport.render(&plan, cols, rows).bytes)?;
        flush_viewport_traces(&mut state.viewport, state.debug.as_deref());
    } else {
        let frame = app.render(cols, rows);
        out(&state.screen.paint(&frame, cols, rows))?;
    }
    Ok(())
}

fn flush_viewport_traces(viewport: &mut ViewportRenderer, debug: Option<&tui::RenderDebug>) {
    let Some(debug) = debug else {
        return;
    };
    let (redraws, dumps) = viewport.take_traces();
    for entry in redraws {
        debug.redraw(&entry);
    }
    for dump in dumps {
        debug.dump(&dump.text());
    }
}

fn compute_wait(app: &App, state: &PaintState, keyboard: &KeyboardProtocol, deferred: Option<Duration>) -> Duration {
    let mut wait = if app.fire_autocomplete_pending() {
        DEBOUNCE
    } else if app.animating() {
        ANIMATION
    } else {
        IDLE
    };
    if let Some(d) = deferred {
        wait = wait.min(d);
    }
    if let Some(at) = app.viewport_deadline().filter(|_| !state.split) {
        wait = wait.min(at.saturating_duration_since(Instant::now()));
    }
    if let Some(at) = keyboard.flush_deadline() {
        wait = wait.min(at.saturating_duration_since(Instant::now()));
    }
    wait
}

fn poll_events(
    app: &mut App,
    events: &Receiver<UiEvent>,
    keyboard: &mut KeyboardProtocol,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
    wait: Duration,
) -> std::io::Result<bool> {
    match events.recv_timeout(wait) {
        Ok(event) => {
            let mut changed = handle(app, event, keyboard, out)?;
            while let Ok(event) = events.try_recv() {
                changed |= handle(app, event, keyboard, out)?;
            }
            Ok(changed)
        }
        Err(RecvTimeoutError::Timeout) => {
            let mut dirty = app.fire_autocomplete();
            if app.animating() {
                app.tick();
                dirty = true;
            }
            Ok(dirty)
        }
        Err(RecvTimeoutError::Disconnected) => {
            app.exit = true;
            Ok(false)
        }
    }
}

fn finish_paint_loop(
    app: &mut App,
    state: &mut PaintState,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let width = state.last_size.0 as usize;
    app.stop_terminal_notifications();
    for write in app.terminal_writes.drain(..) {
        out(&write)?;
    }
    if state.split {
        let plan = app.exit_plan(width);
        out(&state.viewport.render(&plan, width, state.last_size.1 as usize).bytes)?;
        return out("\r\n\x1b[?25h");
    }
    let document = app.exit_document(width);
    if !state.release.is_empty() {
        out(&state.release)?;
    }
    out(&state.screen.exit(&document, width))
}

/// What leaving the TUI for a while (Ctrl+G, Ctrl+Z) writes: bracketed paste and the hidden cursor off, and on the
/// alternate screen its Kitty images deleted (`beforeTerminalStop`) before it is left.
fn leave_bytes(app: &App, split: bool) -> &'static str {
    match (split, app.image_protocol) {
        (true, _) => "\x1b[?2004l\x1b[?25h\r\n",
        (false, Some(tui::ImageProtocol::Kitty)) => "\x1b_Ga=d,d=A,q=2\x1b\\\x1b[?2004l\x1b[?25h\x1b[?1049l",
        (false, _) => "\x1b[?2004l\x1b[?25h\x1b[?1049l",
    }
}

/// `stop()` while the TUI leaves the terminal: pop the Kitty flags, reset modifyOtherKeys, Kitty matching off.
fn leave_keyboard(
    app: &mut App,
    keyboard: &mut KeyboardProtocol,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let bytes = keyboard.stop();
    app.set_kitty_protocol_active(false);
    if bytes.is_empty() { Ok(()) } else { out(&bytes) }
}

/// One event; false when it left the frame as it was (input the app reports unchanged, [`App::handle_input`]).
fn handle(
    app: &mut App,
    event: UiEvent,
    keyboard: &mut KeyboardProtocol,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<bool> {
    match event {
        UiEvent::Input(data) => {
            let negotiated = keyboard.input(&data, Instant::now());
            if !negotiated.write.is_empty() {
                out(&negotiated.write)?;
            }
            let mut changed = negotiated.forward.is_empty();
            if negotiated.kitty_activated {
                app.set_kitty_protocol_active(true);
                changed = true;
            }
            for data in negotiated.forward {
                changed |= app.handle_input(&data);
            }
            return Ok(changed);
        }
        UiEvent::Lane(event) => app.apply(event),
        UiEvent::InputClosed => app.exit = true,
        UiEvent::Wake => {}
    }
    Ok(true)
}
