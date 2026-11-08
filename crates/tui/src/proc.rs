//! Running the small helper programs the terminal features use (clipboard tools, `tmux`, desktop notifiers, `git`,
//! `gh`, `fd`): one bounded run of a program found on the session's PATH.

use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Run `name`, looked up on `path_var` ([`jsrt::process::which`]; without a PATH nothing runs), and return its stdout
/// when it exits 0. `input` is written to its stdin; a program fed input is a writer, which gets no stdout pipe
/// (writers such as `xclip` daemonize and would keep it open). With a `timeout` the program is killed once it runs
/// longer.
pub fn run<S: AsRef<OsStr>>(
    name: &str,
    path_var: Option<&str>,
    args: &[S],
    input: Option<&str>,
    timeout: Option<Duration>,
) -> Option<Vec<u8>> {
    let bound = signals::child::Bound {
        timeout,
        ..Default::default()
    };
    let out = signals::child::run(
        Command::new(jsrt::process::which(name, path_var?)?)
            .args(args)
            .stdout(if input.is_some() { Stdio::null() } else { Stdio::piped() })
            .stderr(Stdio::null()),
        input.map(str::as_bytes),
        bound,
    )
    .ok()?;
    out.success().then_some(out.stdout)
}
