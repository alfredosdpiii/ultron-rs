//! The live RLM views (experimental/rlm-visualizer.ts, rlm-graph.ts, rlm-focus.ts, rlm-pane.ts) as pure model and
//! render code: [`RlmSnapshot`] from the worker's inspections and the lane transcript, the run graph with its
//! one-line footer summary, docked panel and full-screen focus view, the RLM pane of dependency waves with the live
//! wave summary, and the running-work panel between the transcript and the editor. [`RlmPoller`] is the one poll of
//! the inspections they draw from, shared by the native TUI and `ultron watch`.

pub mod focus;
pub mod graph;
pub mod pane;
mod poller;
pub mod running;
pub mod visualizer;

pub use focus::{FocusAction, FocusState, RlmGraphFocus, apply_focus_action, focus_hints, render_focus_view};
pub use graph::{
    GraphKind, GraphNode, GraphRow, GraphStatus, build_rlm_graph, layout_graph, render_rlm_dock, render_rlm_footer,
};
pub use pane::{DagAction, DagRun, PaneEvent, RlmPane, RlmPaneState, build_dag_runs, render_wave_summary};
pub use poller::{Answered, Inspect, MAX_ASSESSED, RlmPoller, TUI_FRAME_LIMIT, WATCH_FRAME_LIMIT};
pub use running::{DetachedCell, RunningWork, parse_detached_cells, render_running_work, running_work};
pub use visualizer::{RlmClock, RlmSnapshot, RlmStyle};

/// JavaScript `String(n)` for the numbers these views print.
pub(crate) fn js_num(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        format!("{n}")
    }
}

/// UTF-16 length (JavaScript `length`).
pub(crate) fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// JavaScript `s.slice(0, n)` on UTF-16 units.
pub(crate) fn js_slice(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

/// `text.replace(/\s+/g, " ").trim()`.
pub(crate) fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `flat(text, limit)`: one line, cut to `limit` UTF-16 units with an ellipsis.
pub(crate) fn flat(text: &str, limit: usize) -> String {
    let line = squash(text);
    if js_len(&line) > limit {
        format!("{}…", js_slice(&line, limit - 1))
    } else {
        line
    }
}

/// chalk's `bold` (`theme.bold`), always on: the themed views are drawn only where colors are.
pub(crate) fn bold(text: &str) -> String {
    tui::theme::styles::TextStyle::Bold.apply(text, 1)
}

/// `theme.fg(color, text)` for a token known to exist.
pub(crate) fn fg(theme: &tui::theme::Theme, color: &str, text: &str) -> String {
    theme.fg(color, text).unwrap_or_else(|_| text.to_string())
}

/// `Number.prototype.toFixed(digits)`: exact decimal expansion, ties rounded away from zero. (A copy of
/// `interactive::components::js_to_fixed`, which this crate cannot depend on.)
pub(crate) fn js_to_fixed(x: f64, digits: usize) -> String {
    let exact = format!("{:.1100}", x.abs());
    let (int, frac) = exact.split_once('.').unwrap_or((&exact, ""));
    let mut d: Vec<u8> = format!("{int}{}", &frac[..digits]).into_bytes();
    if frac.as_bytes()[digits] >= b'5' {
        let mut i = d.len();
        loop {
            if i == 0 {
                d.insert(0, b'1');
                break;
            }
            i -= 1;
            if d[i] == b'9' {
                d[i] = b'0';
            } else {
                d[i] += 1;
                break;
            }
        }
    }
    let s = String::from_utf8(d).unwrap_or_default();
    let (a, b) = s.split_at(s.len() - digits);
    let sign = if x < 0.0 { "-" } else { "" };
    if digits == 0 {
        format!("{sign}{a}")
    } else {
        format!("{sign}{a}.{b}")
    }
}
