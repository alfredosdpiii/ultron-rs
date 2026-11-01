//! Signal handling without `unsafe`: SIGINT, SIGTERM and SIGHUP are caught by `signal-hook` handlers and read on a
//! thread of their own. They are never blocked: a blocked mask is inherited through spawn and exec, so kernels, shells
//! and git ignored SIGTERM (a `timeout` in `bash()` never fired); caught handlers reset to the default on exec.
//!
//! A role or mode installs its handler ([`set_handler`]); it runs on a fresh thread so it may wait (abort a turn,
//! release workers). A second signal while a handler runs exits at once with the signal's exit code. Without a
//! handler the process dies of the signal as it would have unblocked.

use nix::sys::signal::Signal;
use signal_hook::iterator::Signals;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub use nix::sys::signal::Signal as Sig;

/// The signals handled here.
pub const HANDLED: [Signal; 3] = [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP];

pub type Handler = Arc<dyn Fn(Signal) + Send + Sync>;

static HANDLER: Mutex<Option<Handler>> = Mutex::new(None);
static STARTED: OnceLock<()> = OnceLock::new();
static HANDLING: AtomicBool = AtomicBool::new(false);
/// The last handled signal received (its number, set in the signal handler itself), 0 before any.
static RECEIVED: AtomicUsize = AtomicUsize::new(0);

/// The exit code a signal ends a process with (`128 + n`: SIGINT 130, SIGTERM 143, SIGHUP 129).
pub fn exit_code(signal: Signal) -> i32 {
    128 + signal as i32
}

/// Catch the handled signals and start reading them. Call once, first thing in `main`.
pub fn install() -> Result<(), String> {
    if STARTED.get().is_some() {
        return Ok(());
    }
    for signal in HANDLED {
        signal_hook::flag::register_usize(signal as i32, &RECEIVED, signal as usize).map_err(|e| e.to_string())?;
    }
    let mut signals = Signals::new(HANDLED.map(|s| s as i32)).map_err(|e| e.to_string())?;
    let _ = STARTED.set(());
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            for number in signals.forever() {
                if let Ok(signal) = Signal::try_from(number) {
                    dispatch(signal);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn dispatch(signal: Signal) {
    let handler = HANDLER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(handler) = handler else {
        die(signal);
    };
    if HANDLING.swap(true, Ordering::SeqCst) {
        std::process::exit(exit_code(signal));
    }
    std::thread::spawn(move || handler(signal));
}

/// Die of `signal` with its default action, as an uncaught signal would have.
fn die(signal: Signal) -> ! {
    let _ = signal_hook::low_level::emulate_default_handler(signal as i32);
    std::process::exit(exit_code(signal));
}

/// A handled signal this process received (recorded by the signal handler, before any reader runs). Lets a mode
/// tell input that ended because of a signal (the terminal went away) from input that ended on its own.
pub fn received() -> Option<Signal> {
    Signal::try_from(RECEIVED.load(Ordering::SeqCst) as i32).ok()
}

/// Handle the signals with `handler` from now on (replacing any previous one).
pub fn set_handler(handler: Handler) {
    *HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
}

/// Back to the default action.
pub fn clear_handler() {
    *HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = None;
}
