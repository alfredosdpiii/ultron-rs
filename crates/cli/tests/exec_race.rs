//! Scripts written by tests and then executed by path stay executable while other test threads fork
//! (`executable_support`): the race that made fake `claude`/`gh`/linter launches fail with ETXTBSY ("Text file
//! busy") under parallel load. Every wait here is a process ending; nothing sleeps.

use crate::executable_support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const WRITERS: usize = 4;
const FORKERS: usize = 4;
const ROUNDS: usize = 150;

/// Install-then-exec `ROUNDS` scripts in each of `WRITERS` threads while `FORKERS` threads fork without pause; the
/// failed launches, with their errors.
fn launch_while_forking(dir: &Path) -> Vec<String> {
    let stop = Arc::new(AtomicBool::new(false));
    let forkers: Vec<_> = (0..FORKERS)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("/bin/true").status();
                }
            })
        })
        .collect();
    let writers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let dir: PathBuf = dir.to_path_buf();
            std::thread::spawn(move || {
                let mut failures = Vec::new();
                for round in 0..ROUNDS {
                    let script = dir.join(format!("s{writer}-{round}"));
                    executable_support::install(&script, "#!/bin/sh\nexit 7\n");
                    match Command::new(&script).status() {
                        Ok(status) if status.code() == Some(7) => {}
                        Ok(status) => failures.push(format!("{}: {status}", script.display())),
                        Err(error) => failures.push(format!("{}: {error}", script.display())),
                    }
                }
                failures
            })
        })
        .collect();
    let failures = writers.into_iter().flat_map(|w| w.join().unwrap()).collect();
    stop.store(true, Ordering::Relaxed);
    for forker in forkers {
        forker.join().unwrap();
    }
    failures
}

#[test]
fn cli_exec_race_installed_scripts_always_launch_while_other_threads_fork() {
    let dir = tempfile::tempdir().unwrap();
    let failures = launch_while_forking(dir.path());
    assert!(
        failures.is_empty(),
        "{} launches failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
