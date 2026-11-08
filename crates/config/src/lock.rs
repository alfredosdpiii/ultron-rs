//! `proper-lockfile`-compatible locks: the lock is the directory `<file>.lock`, created atomically with `mkdir`.
//! A lock directory whose mtime is older than `stale` is taken over, as `proper-lockfile` does; a holder that asks
//! for it refreshes the mtime while it holds the lock (`update`), so a long hold never goes stale. TypeScript and
//! Rust holders exclude each other.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

/// Held lock; dropping it stops its refresh and removes the lock directory.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
    /// Dropped to stop the mtime refresh.
    refresh: Option<mpsc::Sender<()>>,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.refresh.take();
        let _ = fs::remove_dir(&self.path);
    }
}

/// How a lock is waited for and held.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// A lock directory not refreshed for longer is taken over.
    pub stale: Duration,
    pub retry: Retry,
    /// While held, the lock's mtime is refreshed this often (`proper-lockfile`'s `update`); `None`: never.
    pub update: Option<Duration>,
}

/// How long another holder is waited for: tries `delay` apart (a lock directory has no event to wait on).
#[derive(Clone, Copy, Debug)]
pub struct Retry {
    pub delay: Duration,
    /// The delay doubles after each try, up to this; `None`: it stays `delay`.
    pub backoff_to: Option<Duration>,
    pub until: Until,
}

#[derive(Clone, Copy, Debug)]
pub enum Until {
    /// This many tries in all.
    Tries(u32),
    /// Tries until this long has passed.
    Elapsed(Duration),
}

/// Settings and trust locks: `lockSync` retried 10 times, 20 ms apart, stale after 10 s.
pub const SYNC: Options = Options {
    stale: Duration::from_secs(10),
    retry: Retry {
        delay: Duration::from_millis(20),
        backoff_to: None,
        until: Until::Tries(10),
    },
    update: None,
};

fn try_acquire(lock_path: &Path, stale: Duration) -> io::Result<()> {
    match fs::create_dir(lock_path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        Err(_) => {}
    }
    let modified = match fs::metadata(lock_path).and_then(|m| m.modified()) {
        Ok(modified) => modified,
        // Released between our mkdir and stat: try once more without the stale check.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return try_acquire(lock_path, Duration::MAX),
        Err(e) => return Err(e),
    };
    let age = SystemTime::now().duration_since(modified).unwrap_or_default();
    if age <= stale {
        // `proper-lockfile`'s ELOCKED error.
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Lock file is already being held",
        ));
    }
    fs::remove_dir_all(lock_path)?;
    try_acquire(lock_path, Duration::MAX)
}

/// Locks `lock_path` (the `.lock` directory), retrying as `options.retry` says while another holder has it. A lock
/// still held after the last try is an error of kind `WouldBlock`.
pub fn acquire(lock_path: &Path, options: Options) -> io::Result<LockGuard> {
    let started = Instant::now();
    let mut tries = 1;
    let mut delay = options.retry.delay;
    loop {
        match try_acquire(lock_path, options.stale) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let more = match options.retry.until {
                    Until::Tries(count) => tries < count,
                    Until::Elapsed(wait) => started.elapsed() < wait,
                };
                if !more {
                    return Err(e);
                }
                tries += 1;
                sleep(delay);
                if let Some(most) = options.retry.backoff_to {
                    delay = (delay * 2).min(most);
                }
            }
            Err(e) => return Err(e),
            Ok(()) => break,
        }
    }
    let refresh = options.update.map(|every| {
        let (stop, stopped) = mpsc::channel::<()>();
        let touch = lock_path.to_path_buf();
        std::thread::spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(every) {
                // A lost lock at worst lets two holders overlap (`onCompromised` is ignored there too).
                if fs::File::open(&touch)
                    .and_then(|dir| dir.set_modified(SystemTime::now()))
                    .is_err()
                {
                    return;
                }
            }
        });
        stop
    });
    Ok(LockGuard {
        path: lock_path.to_path_buf(),
        refresh,
    })
}

/// The lock directory `proper-lockfile` uses for `file` with `realpath: false`.
pub fn lock_path_for(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}
