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

use jsrt::{string, utf16};

/// `flat(text, limit)`: one line, cut to `limit` UTF-16 units with an ellipsis.
pub(crate) fn flat(text: &str, limit: usize) -> String {
    let line = string::flatten(text);
    if utf16::len(&line) > limit {
        format!("{}…", utf16::head_lossy(&line, limit - 1))
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
