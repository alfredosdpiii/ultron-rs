use signals::child::{self, Bound, End, Stop};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn sh(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

const LONG: Option<Duration> = Some(Duration::from_secs(60));

#[test]
fn output_larger_than_a_pipe_buffer_is_drained_while_the_child_runs() {
    let bound = Bound {
        timeout: LONG,
        ..Bound::default()
    };
    let out = child::run(&mut sh("head -c 1000000 /dev/zero; echo done >&2"), None, bound).unwrap();
    assert!(out.success());
    assert_eq!(out.stdout.len(), 1_000_000);
    assert_eq!(out.stderr, b"done\n");
    assert!(!out.cut);
}

#[test]
fn input_is_fed_to_stdin_and_closed() {
    let out = child::run(&mut sh("cat"), Some(b"hello"), Bound::default()).unwrap();
    assert_eq!(out.code(), Some(0));
    assert_eq!(out.stdout, b"hello");
}

#[test]
fn a_child_past_its_timeout_is_stopped() {
    let started = Instant::now();
    let bound = Bound {
        timeout: Some(Duration::from_millis(100)),
        ..Bound::default()
    };
    let out = child::run(&mut sh("exec sleep 30"), None, bound).unwrap();
    assert_eq!(out.end, End::TimedOut);
    assert!(out.status().is_none());
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn cancel_from_another_thread_stops_the_child() {
    let running = child::start(&mut sh("exec sleep 30"), None, Bound::default()).unwrap();
    let watched = running.watched().clone();
    std::thread::spawn(move || watched.cancel());
    let out = running.finish();
    assert_eq!(out.end, End::Cancelled);
}

#[test]
fn a_group_stop_terminates_what_the_child_started() {
    let bound = Bound {
        timeout: Some(Duration::from_millis(100)),
        stop: Stop {
            group: true,
            term_grace: Some(Duration::from_secs(5)),
        },
        keep: None,
    };
    let started = Instant::now();
    // The background sleep holds stdout open: only a group stop closes it before the pipe grace.
    let out = child::run(&mut sh("sleep 30 & wait"), None, bound).unwrap();
    assert_eq!(out.end, End::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
}

#[test]
fn keep_holds_the_last_bytes_and_reports_the_cut() {
    let bound = Bound {
        keep: Some(4),
        ..Bound::default()
    };
    let out = child::run(&mut sh("head -c 200000 /dev/zero; printf tail"), None, bound).unwrap();
    assert!(out.success());
    assert_eq!(out.stdout, b"tail");
    assert!(out.cut);
}

#[test]
fn a_leftover_process_holding_the_pipe_does_not_hold_the_run() {
    let started = Instant::now();
    let out = child::run(&mut sh("printf early; sleep 10 &"), None, Bound::default()).unwrap();
    assert!(out.success());
    assert_eq!(out.stdout, b"early");
    assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
}
