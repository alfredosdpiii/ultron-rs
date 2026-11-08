//! History/viewport split, frame gate and the bounded-work benchmark (contract from omp
//! docs/tui-core-renderer.md and packages/tui/test/{history-frame-plan,output-backpressure-gate}.test.ts).

use crate::support;

use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use support::latch::HANG_GUARD;
use support::vterm::VTerm;
use tui::CURSOR_MARKER;
use tui::TtyWriter;
use tui::*;

const R: &str = "\x1b[0m\x1b]8;;\x07";

fn plan(viewport: &[&str]) -> FramePlan {
    FramePlan {
        history: None,
        viewport: viewport.iter().map(|s| s.to_string()).collect(),
    }
}

fn with_history(id: u64, rows: &[&str], viewport: &[&str]) -> FramePlan {
    FramePlan {
        history: Some(HistoryBatch {
            id,
            rows: rows.iter().map(|s| s.to_string()).collect(),
            kind: HistoryKind::Append,
        }),
        ..plan(viewport)
    }
}

#[test]
fn tui_viewport_first_frame_then_diffs_only_changed_rows() {
    let mut r = ViewportRenderer::new();
    let f = r.render(&plan(&["a", "b"]), 10, 5);
    assert_eq!(f.bytes, format!("\x1b[?2026ha{R}\r\nb{R}\x1b[?25l\x1b[?2026l"));
    assert_eq!(f.acknowledged, None);
    let f = r.render(&plan(&["a", "c"]), 10, 5);
    assert_eq!(f.bytes, format!("\x1b[?2026h\r\x1b[2Kc{R}\x1b[?25l\x1b[?2026l"));
    let f = r.render(&plan(&["x", "c"]), 10, 5);
    assert_eq!(f.bytes, format!("\x1b[?2026h\x1b[1A\r\x1b[2Kx{R}\x1b[?25l\x1b[?2026l"));
    // unchanged: nothing but the cursor state
    let f = r.render(&plan(&["x", "c"]), 10, 5);
    assert_eq!(f.bytes, "\x1b[?2026h\x1b[?25l\x1b[?2026l");
    // growth extends below with CRLF; shrink erases below
    let f = r.render(&plan(&["x", "c", "d"]), 10, 5);
    assert_eq!(
        f.bytes,
        format!("\x1b[?2026h\x1b[1B\r\n\r\x1b[2Kd{R}\x1b[?25l\x1b[?2026l")
    );
    let f = r.render(&plan(&["x"]), 10, 5);
    assert_eq!(f.bytes, "\x1b[?2026h\x1b[1A\r\x1b[J\x1b[?25l\x1b[?2026l");
    assert_eq!(r.rows_written, 2 + 1 + 1 + 1);
    // shrinking by exactly one row erases it
    r.render(&plan(&["x", "y"]), 10, 5);
    let f = r.render(&plan(&["x"]), 10, 5);
    assert_eq!(f.bytes, "\x1b[?2026h\r\x1b[J\x1b[?25l\x1b[?2026l");
}

#[test]
fn tui_viewport_clamps_wide_rows_and_places_the_cursor() {
    let mut r = ViewportRenderer::new();
    r.show_hardware_cursor = true;
    let f = r.render(&plan(&["abcdefghij", &format!("ab{CURSOR_MARKER}c")]), 4, 5);
    assert_eq!(
        f.bytes,
        format!("\x1b[?2026habcd\x1b[0m{R}\r\nabc{R}\x1b[3G\x1b[?25h\x1b[?2026l")
    );
    assert_eq!(r.viewport(), [format!("abcd\x1b[0m{R}"), format!("abc{R}")]);
}

#[test]
fn tui_viewport_appends_history_once_and_acknowledges() {
    let mut vt = VTerm::new(20, 4);
    let mut r = ViewportRenderer::new();
    let f = r.render(&plan(&["live", "> edit"]), 20, 4);
    vt.write(&f.bytes);
    let f = r.render(&with_history(1, &["msg 1", "msg 2"], &["live", "> edit"]), 20, 4);
    assert_eq!(f.acknowledged, Some(1));
    vt.write(&f.bytes);
    assert_eq!(vt.all_lines(), ["msg 1", "msg 2", "live", "> edit"]);
    // The same batch offered again (ack lost or coalesced render) is acknowledged but never rewritten.
    let f = r.render(&with_history(1, &["msg 1", "msg 2"], &["live", "> edit"]), 20, 4);
    assert_eq!(f.acknowledged, Some(1));
    assert_eq!(f.bytes, "\x1b[?2026h\x1b[?25l\x1b[?2026l");
    vt.write(&f.bytes);
    // More history than the screen holds: the oldest rows scroll into scrollback in order.
    let f = r.render(
        &with_history(2, &["msg 3", "msg 4", "msg 5"], &["live 2", "> edit"]),
        20,
        4,
    );
    vt.write(&f.bytes);
    assert_eq!(
        vt.all_lines(),
        ["msg 1", "msg 2", "msg 3", "msg 4", "msg 5", "live 2", "> edit"]
    );
    assert_eq!(vt.screen_lines(), ["msg 4", "msg 5", "live 2", "> edit"]);
    assert_eq!(vt.scrollback, ["msg 1", "msg 2", "msg 3"]);
    assert_eq!(r.history_rows_written, 5);
    // Ordinary frames then only touch the viewport rows.
    let f = r.render(&plan(&["live 3", "> edit"]), 20, 4);
    vt.write(&f.bytes);
    assert_eq!(vt.screen_lines(), ["msg 4", "msg 5", "live 3", "> edit"]);
    assert_eq!(vt.scrollback, ["msg 1", "msg 2", "msg 3"]);
}

#[test]
fn tui_viewport_replay_clears_scrollback_and_rewrites_the_ledger() {
    let mut vt = VTerm::new(20, 4);
    let mut r = ViewportRenderer::new();
    vt.write(
        &r.render(&with_history(1, &["old 1", "old 2", "old 3"], &["v"]), 20, 4)
            .bytes,
    );
    let replay = FramePlan {
        history: Some(HistoryBatch {
            id: 2,
            rows: vec!["new 1".into(), "new 2".into()],
            kind: HistoryKind::Replay,
        }),
        viewport: vec!["v".into()],
    };
    let f = r.render(&replay, 20, 4);
    assert!(f.bytes.starts_with("\x1b[?2026h\x1b[H\x1b[2J\x1b[3J"));
    vt.write(&f.bytes);
    assert_eq!(vt.all_lines(), ["new 1", "new 2", "v"]);
}

#[test]
fn tui_viewport_resize_repaints_the_viewport_only() {
    let mut vt = VTerm::new(20, 5);
    let mut r = ViewportRenderer::new();
    vt.write(&r.render(&with_history(1, &["h1"], &["a", "b", "c"]), 20, 5).bytes);
    let f = r.render(&plan(&["a", "b", "c"]), 18, 5);
    assert!(f.bytes.starts_with("\x1b[?2026h\x1b[2A\r\x1b[J"));
    vt.write(&f.bytes);
    assert_eq!(vt.all_lines(), ["h1", "a", "b", "c"]);
    // invalidate forces a full viewport repaint without history
    r.invalidate();
    let f = r.render(&plan(&["a", "b", "c"]), 18, 5);
    assert_eq!(r.rows_written, 9 + 3 - 3);
    vt.write(&f.bytes);
    assert_eq!(vt.all_lines(), ["h1", "a", "b", "c"]);
}

#[test]
fn tui_viewport_gate_paces_frames_and_defers_on_backlog() {
    let mut g = RenderGate::new();
    let t0 = Instant::now();
    assert_eq!(g.poll(t0, None), GateDecision::Idle);
    g.request();
    assert_eq!(g.poll(t0, Some(0)), GateDecision::Render);
    assert!(!g.requested());
    g.rendered(Duration::from_millis(1));
    g.request();
    // 30 fps cadence
    assert_eq!(
        g.poll(t0 + Duration::from_millis(10), None),
        GateDecision::Wait(Duration::from_micros(23_333))
    );
    assert_eq!(g.poll(t0 + Duration::from_millis(34), None), GateDecision::Render);
    // adaptive duty cycle: a 50 ms frame defers the next until 100 ms after its start
    g.rendered(Duration::from_millis(50));
    g.request();
    let t1 = t0 + Duration::from_millis(34);
    assert_eq!(
        g.poll(t1 + Duration::from_millis(40), None),
        GateDecision::Wait(Duration::from_millis(60))
    );
    // capped at 200 ms
    g.rendered(Duration::from_secs(5));
    assert_eq!(
        g.poll(t1 + Duration::from_millis(150), None),
        GateDecision::Wait(Duration::from_millis(50))
    );
    assert_eq!(g.poll(t1 + Duration::from_millis(200), None), GateDecision::Render);
    // backlog above 256 KiB: skip and retry in 10 ms, keeping the request
    g.rendered(Duration::ZERO);
    g.request();
    let t2 = t1 + Duration::from_secs(1);
    assert_eq!(
        g.poll(t2, Some(256 * 1024 + 1)),
        GateDecision::Wait(Duration::from_millis(10))
    );
    assert!(g.requested());
    assert_eq!(g.skipped, 1);
    assert_eq!(g.poll(t2, Some(256 * 1024)), GateDecision::Render);
}

/// End to end: a stalled terminal backs the writer up; frames composed meanwhile are skipped and the frame painted
/// after the drain carries only the latest state (omp output-backpressure-gate.test.ts).
#[test]
fn tui_viewport_skips_stale_frames_while_the_writer_is_backlogged() {
    #[derive(Clone)]
    struct Stalled(Arc<(Mutex<bool>, Condvar)>, Arc<Mutex<Vec<u8>>>);
    impl Write for Stalled {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut open = self.0.0.lock().unwrap();
            while !*open {
                open = self.0.1.wait(open).unwrap();
            }
            self.1.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let term = Stalled(
        Arc::new((Mutex::new(true), Condvar::new())),
        Arc::new(Mutex::new(Vec::new())),
    );
    let mut writer = TtyWriter::new(term.clone()).unwrap();
    let mut renderer = ViewportRenderer::new();
    let mut gate = RenderGate::new();
    gate.min_interval = Duration::ZERO;
    let mut paint = |state: &str, gate: &mut RenderGate, writer: &TtyWriter, now: Instant| {
        gate.request();
        match gate.poll(now, Some(writer.pending())) {
            GateDecision::Render => {
                writer.write(&renderer.render(&plan(&[state]), 40, 6).bytes);
                gate.rendered(Duration::ZERO);
                true
            }
            _ => false,
        }
    };
    let now = Instant::now();
    assert!(paint("initial", &mut gate, &writer, now));
    assert!(writer.flush_sync(HANG_GUARD));
    term.1.lock().unwrap().clear();

    // Stall the terminal behind a 1 MiB backlog.
    *term.0.0.lock().unwrap() = false;
    writer.write(&"x".repeat(1024 * 1024));
    assert!(!paint("frame-A", &mut gate, &writer, now));
    assert!(!paint("frame-B", &mut gate, &writer, now));
    assert_eq!(gate.skipped, 2);

    *term.0.0.lock().unwrap() = true;
    term.0.1.notify_all();
    assert!(writer.flush_sync(HANG_GUARD));
    assert!(paint("frame-B", &mut gate, &writer, now));
    assert!(writer.flush_sync(HANG_GUARD));
    let out = String::from_utf8(term.1.lock().unwrap().clone()).unwrap();
    assert!(out.contains("frame-B"));
    assert!(!out.contains("frame-A"));
    writer.stop(HANG_GUARD);
}
