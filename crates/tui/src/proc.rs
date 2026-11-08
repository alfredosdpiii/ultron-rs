//! Running the small helper programs the terminal features use (clipboard tools, `tmux`, desktop notifiers, `git`,
//! `gh`, `fd`): the PATH lookup and one bounded run.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `name` on `path_var` (a `PATH` value): the first regular file there, executable on Unix.
pub fn which(name: &str, path_var: Option<&str>) -> Option<PathBuf> {
    std::env::split_paths(path_var?).map(|dir| dir.join(name)).find(|p| {
        let Ok(meta) = std::fs::metadata(p) else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                return false;
            }
        }
        meta.is_file()
    })
}

/// Run `name`, looked up on `path_var` ([`which`]; without a PATH nothing runs), and return its stdout when it
/// exits 0. `input` is written to its stdin; a program fed input is a
/// writer, which gets no stdout pipe (writers such as `xclip` daemonize and would keep it open). With a `timeout`
/// the program is killed once it runs longer.
pub fn run<S: AsRef<OsStr>>(
    name: &str,
    path_var: Option<&str>,
    args: &[S],
    input: Option<&str>,
    timeout: Option<Duration>,
) -> Option<Vec<u8>> {
    let mut child = Command::new(which(name, path_var)?)
        .args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(if input.is_some() { Stdio::null() } else { Stdio::piped() })
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A writer may exit before consuming all input.
        let _ = stdin.write_all(input.unwrap_or("").as_bytes());
    }
    let reader = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = stdout.read_to_end(&mut out);
            out
        })
    });
    let status = match timeout {
        None => child.wait().ok(),
        Some(timeout) => {
            let deadline = Instant::now() + timeout;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    // `Child` has no wait with a deadline, and killing a child another thread waits on needs its
                    // handle, so this one poll loop stands in for both.
                    Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                }
            }
        }
    };
    let out = reader.and_then(|r| r.join().ok()).unwrap_or_default();
    status.filter(|s| s.success()).map(|_| out)
}
