//! `signals`: children of a process that installed the handlers see default signal handling (nothing
//! blocked: a `timeout` or a test's SIGTERM reaches them), and a signal still reaches the handler. Its own test
//! binary: `install` is once per process.

use signals::{Sig, install, received, set_handler};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

#[test]
fn signals_leave_children_unblocked_and_reach_the_handler() {
    install().unwrap();
    let status = std::process::Command::new("grep")
        .args(["SigBlk", "/proc/self/status"])
        .output()
        .unwrap();
    let line = String::from_utf8(status.stdout).unwrap();
    let blocked = u64::from_str_radix(line.split(':').nth(1).unwrap().trim(), 16).unwrap();
    for signal in [Sig::SIGINT, Sig::SIGTERM, Sig::SIGHUP] {
        assert_eq!(
            blocked & (1 << (signal as i32 - 1)),
            0,
            "{signal} blocked in a child: {line}"
        );
    }

    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    set_handler(Arc::new(move |signal| tx.lock().unwrap().send(signal).unwrap()));
    nix::sys::signal::raise(Sig::SIGHUP).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Sig::SIGHUP);
    assert_eq!(received(), Some(Sig::SIGHUP));
}
