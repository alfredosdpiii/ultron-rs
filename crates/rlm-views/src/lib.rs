//! The live RLM views (experimental/rlm-visualizer.ts, rlm-graph.ts, rlm-focus.ts, rlm-pane.ts) as pure model and
//! render code: [`visualizer::RlmSnapshot`] from the worker's inspections and the lane transcript, the run graph with
//! its one-line footer summary, docked panel and full-screen focus view, the RLM pane of dependency waves with the
//! live wave summary, and the running-work panel between the transcript and the editor. [`poller::RlmPoller`] is the
//! one poll of the inspections they draw from, shared by the native TUI and `ultron watch`.

pub mod focus;
pub mod graph;
pub mod pane;
pub mod poller;
pub mod running;
pub mod visualizer;

use jsrt::{string, utf16};

/// One line, cut to `limit` UTF-16 units with an ellipsis.
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
    theme::styles::TextStyle::Bold.apply(text, 1)
}
