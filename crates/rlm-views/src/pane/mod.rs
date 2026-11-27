//! The RLM pane (experimental/rlm-pane.ts): the run as dependency waves in the side pane (`app.rlm.pane`), and the
//! live wave summary above the editor. A run is the current root turn (each cell's work is one wave, a child's work
//! follows it) or one `workflows.run` call (waves are the topological levels of `dependsOn`).

mod build;
mod render;

use std::collections::{HashMap, HashSet};

use tui::KeybindingsManager;

use super::graph::{GraphKind, GraphProgress, GraphStatus, is_finished};
use super::visualizer::{RlmSnapshot, RlmStyle};

pub use build::{build_dag_runs, compute_waves, default_run, prompt_of};
pub use render::{
    Body, DagRenderOptions, PaneRender, pack_hints, render_dag_body, render_rlm_pane, render_wave_summary, status_word,
    wave_geometry,
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DagNode {
    pub key: String,
    pub name: String,
    pub kind: Option<GraphKind>,
    pub status: Option<GraphStatus>,
    /// Keys of the nodes this one waits for, within its run.
    pub depends_on: Vec<String>,
    /// Shown when the node has no dependencies (e.g. "Start node", "cell 2").
    pub origin: String,
    pub prompt: Option<String>,
    pub category: String,
    pub model: Option<String>,
    pub text: Option<String>,
    pub note: Option<String>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub turns: Option<f64>,
    pub tool_calls: Option<f64>,
    pub progress: Option<GraphProgress>,
}

impl DagNode {
    pub fn status(&self) -> GraphStatus {
        self.status.unwrap_or(GraphStatus::Pending)
    }
    pub fn kind(&self) -> GraphKind {
        self.kind.unwrap_or(GraphKind::Task)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    Turn,
    Workflow,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DagRun {
    /// "turn" or `workflow:<run>`.
    pub key: String,
    pub kind: RunKind,
    pub id: String,
    pub title: String,
    pub nodes: Vec<DagNode>,
    /// Node keys per wave, in display order.
    pub waves: Vec<Vec<String>>,
    /// Dependency edges as (from, to) node keys.
    pub edges: Vec<(String, String)>,
    pub status: GraphStatus,
    pub started_at: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunCounts {
    pub total: usize,
    pub done: usize,
    pub running: usize,
    pub pending: usize,
    pub failed: usize,
    /// 1-based index of the first wave with unfinished work.
    pub wave: usize,
    pub waves: usize,
}

pub fn run_counts(run: &DagRun) -> RunCounts {
    let statuses: HashMap<&str, GraphStatus> = run.nodes.iter().map(|n| (n.key.as_str(), n.status())).collect();
    let mut c = RunCounts {
        total: run.nodes.len(),
        done: 0,
        running: 0,
        pending: 0,
        failed: 0,
        wave: 0,
        waves: run.waves.len(),
    };
    for n in &run.nodes {
        match n.status() {
            GraphStatus::Done => c.done += 1,
            GraphStatus::Running => c.running += 1,
            GraphStatus::Pending => c.pending += 1,
            GraphStatus::Failed => c.failed += 1,
            _ => {}
        }
    }
    let open = run.waves.iter().position(|w| {
        w.iter()
            .any(|k| !is_finished(statuses.get(k.as_str()).copied().unwrap_or(GraphStatus::Done)))
    });
    c.wave = open.map_or(run.waves.len(), |i| i + 1);
    c
}

#[derive(Clone, Debug, PartialEq)]
pub struct RlmPaneState {
    /// Selected run (sticky while it exists); the default run otherwise.
    pub run_key: Option<String>,
    pub selected_key: Option<String>,
    /// Folded boxes (one line instead of three).
    pub folded: HashSet<String>,
    /// Node detail cards shown.
    pub details: bool,
    /// First visible body line.
    pub scroll: usize,
    /// Scroll the selected node into view on the next render.
    pub reveal: bool,
}

impl Default for RlmPaneState {
    fn default() -> Self {
        RlmPaneState {
            run_key: None,
            selected_key: None,
            folded: HashSet::new(),
            details: true,
            scroll: 0,
            reveal: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DagAction {
    Next,
    Prev,
    Fold,
    Details,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    PrevRun,
    NextRun,
}

pub fn current_run<'a>(runs: &'a [DagRun], state: &RlmPaneState) -> Option<&'a DagRun> {
    state
        .run_key
        .as_deref()
        .and_then(|k| runs.iter().find(|r| r.key == k))
        .or_else(|| default_run(runs))
}

/// Nodes in navigation order: wave by wave, left to right.
pub fn node_order(run: &DagRun) -> Vec<String> {
    run.waves.iter().flatten().cloned().collect()
}

/// The selected node key: the chosen one while it exists, else the first running node, else the first node.
pub fn selected_node(run: &DagRun, state: &RlmPaneState) -> Option<String> {
    let order = node_order(run);
    if let Some(k) = &state.selected_key
        && order.contains(k)
    {
        return Some(k.clone());
    }
    let by_key: HashMap<&str, &DagNode> = run.nodes.iter().map(|n| (n.key.as_str(), n)).collect();
    order
        .iter()
        .find(|k| {
            by_key
                .get(k.as_str())
                .is_some_and(|n| n.status() == GraphStatus::Running)
        })
        .or(order.first())
        .cloned()
}

fn step_node(state: &RlmPaneState, run: &DagRun, order: &[String], index: Option<usize>, step: isize) -> RlmPaneState {
    if order.is_empty() {
        return state.clone();
    }
    let n = order.len() as isize;
    let at = index.map_or(0, |i| ((i as isize + step + n) % n) as usize);
    let mut next = state.clone();
    next.run_key = Some(run.key.clone());
    next.selected_key = Some(order[at].clone());
    next.reveal = true;
    next
}

fn fold_selected(state: &RlmPaneState, run: &DagRun, selected: Option<String>) -> RlmPaneState {
    let Some(sel) = selected else { return state.clone() };
    let mut next = state.clone();
    if !next.folded.remove(&sel) {
        next.folded.insert(sel.clone());
    }
    next.run_key = Some(run.key.clone());
    next.selected_key = Some(sel);
    next.reveal = true;
    next
}

fn switch_run(state: &RlmPaneState, runs: &[DagRun], run: &DagRun, step: isize) -> RlmPaneState {
    if runs.len() < 2 {
        return state.clone();
    }
    let at = runs.iter().position(|r| r.key == run.key).unwrap_or(0) as isize;
    let n = runs.len() as isize;
    let target = &runs[((at + step + n) % n) as usize];
    let mut next = state.clone();
    next.selected_key = None;
    next.run_key = Some(target.key.clone());
    next.scroll = 0;
    next.reveal = true;
    next
}

pub fn apply_dag_action(state: &RlmPaneState, runs: &[DagRun], action: DagAction, page: usize) -> RlmPaneState {
    let Some(run) = current_run(runs, state) else {
        return state.clone();
    };
    let order = node_order(run);
    let selected = selected_node(run, state);
    let index = selected.as_ref().and_then(|s| order.iter().position(|k| k == s));
    let page = page.max(1);
    match action {
        DagAction::Next => step_node(state, run, &order, index, 1),
        DagAction::Prev => step_node(state, run, &order, index, -1),
        DagAction::Fold => fold_selected(state, run, selected),
        DagAction::PrevRun => switch_run(state, runs, run, -1),
        DagAction::NextRun => switch_run(state, runs, run, 1),
        DagAction::Details => {
            let mut next = state.clone();
            next.details = !state.details;
            next.reveal = true;
            next
        }
        DagAction::ScrollUp | DagAction::ScrollDown | DagAction::PageUp | DagAction::PageDown => {
            let mut next = state.clone();
            next.scroll = match action {
                DagAction::ScrollUp => state.scroll.saturating_sub(1),
                DagAction::ScrollDown => state.scroll + 1,
                DagAction::PageUp => state.scroll.saturating_sub(page),
                _ => state.scroll + page,
            };
            next.reveal = false;
            next
        }
    }
}

pub(crate) fn summarized(kind: GraphKind) -> bool {
    matches!(
        kind,
        GraphKind::Task | GraphKind::Child | GraphKind::Background | GraphKind::Workflow | GraphKind::Fanout
    )
}

/// The runs whose work the RLM pane shows and that count as new work for its auto-open.
pub fn pane_work_runs(runs: &[DagRun], since: Option<f64>) -> Vec<&DagRun> {
    runs.iter()
        .filter(|run| {
            let nodes: Vec<&DagNode> = run.nodes.iter().filter(|n| summarized(n.kind())).collect();
            if nodes.is_empty() {
                return false;
            }
            if nodes.iter().any(|n| build::is_active(n.status())) {
                return true;
            }
            let Some(since) = since else { return false };
            run.kind == RunKind::Turn || run.started_at.is_some_and(|s| s >= since - 1000.0)
        })
        .collect()
}

const PANE_ACTIONS: [(Option<DagAction>, &str); 11] = [
    (None, "app.rlm.pane.close"),
    (Some(DagAction::Next), "app.rlm.pane.next"),
    (Some(DagAction::Prev), "app.rlm.pane.prev"),
    (Some(DagAction::Fold), "app.rlm.pane.fold"),
    (Some(DagAction::Details), "app.rlm.pane.details"),
    (Some(DagAction::ScrollUp), "app.rlm.pane.scrollUp"),
    (Some(DagAction::ScrollDown), "app.rlm.pane.scrollDown"),
    (Some(DagAction::PageUp), "app.rlm.pane.pageUp"),
    (Some(DagAction::PageDown), "app.rlm.pane.pageDown"),
    (Some(DagAction::PrevRun), "app.rlm.pane.prevRun"),
    (Some(DagAction::NextRun), "app.rlm.pane.nextRun"),
];

const KEY_NAMES: &[(&str, &str)] = &[
    ("tab", "Tab"),
    ("shift+tab", "Shift-Tab"),
    ("space", "Space"),
    ("enter", "Enter"),
    ("escape", "Esc"),
    ("up", "↑"),
    ("down", "↓"),
    ("left", "←"),
    ("right", "→"),
    ("pageUp", "PgUp"),
    ("pageDown", "PgDn"),
];

/// A key as the pane shows it: "Tab", "Shift-Tab", "↑"; others as they are.
pub fn key_name(key: &str) -> String {
    KEY_NAMES
        .iter()
        .find(|(k, _)| *k == key)
        .map_or(key, |(_, v)| *v)
        .to_string()
}

fn keys_of(keys: &KeybindingsManager, action: &str) -> Vec<String> {
    keys.get_keys(action).iter().map(|k| key_name(k)).collect()
}

/// Footer hints from the live bindings: "Tab/n next", "Shift-Tab/p prev", "Space/Enter fold", "d Details", ...
pub fn rlm_pane_hints(keys: &KeybindingsManager) -> Vec<String> {
    let label = |a: &str| {
        let k = keys_of(keys, a);
        if k.is_empty() {
            "unbound".to_string()
        } else {
            k.join("/")
        }
    };
    let pair = |first: &str, second: &str| {
        let a = keys_of(keys, first).into_iter().next().unwrap_or_else(|| "?".into());
        let b = keys_of(keys, second).into_iter().next().unwrap_or_else(|| "?".into());
        if a.encode_utf16().count() == 1 && b.encode_utf16().count() == 1 {
            format!("{a}{b}")
        } else {
            format!("{a}/{b}")
        }
    };
    vec![
        format!("{} next", label("app.rlm.pane.next")),
        format!("{} prev", label("app.rlm.pane.prev")),
        format!("{} fold", label("app.rlm.pane.fold")),
        format!("{} Details", label("app.rlm.pane.details")),
        format!("{} Scroll", pair("app.rlm.pane.scrollUp", "app.rlm.pane.scrollDown")),
        format!("{} Runs", pair("app.rlm.pane.prevRun", "app.rlm.pane.nextRun")),
        format!("{} Close", label("app.rlm.pane.close")),
    ]
}

/// What a key did in the pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneEvent {
    Ignored,
    Render,
    Close,
}

/// The RLM pane's state across frames.
#[derive(Clone, Debug, Default)]
pub struct RlmPane {
    pub state: RlmPaneState,
}

impl RlmPane {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle a key against the current snapshot; `height` is the pane's height (a page is `height - 12` lines).
    pub fn handle_input(
        &mut self,
        keys: &KeybindingsManager,
        data: &str,
        snapshot: &RlmSnapshot,
        height: usize,
    ) -> PaneEvent {
        let Some(action) = PANE_ACTIONS
            .iter()
            .find(|(_, id)| keys.matches(data, id))
            .map(|(a, _)| *a)
        else {
            return PaneEvent::Ignored;
        };
        let Some(action) = action else {
            return PaneEvent::Close;
        };
        let runs = build_dag_runs(snapshot);
        self.state = apply_dag_action(&self.state, &runs, action, height.saturating_sub(12).max(1));
        PaneEvent::Render
    }

    /// Render `height` lines of `width` columns (one column of left padding next to the split border).
    #[expect(
        clippy::too_many_arguments,
        reason = "internal wiring helper; parameters match distinct caller inputs"
    )]
    pub fn render(
        &mut self,
        keys: &KeybindingsManager,
        snapshot: &RlmSnapshot,
        width: usize,
        height: usize,
        focused: bool,
        toggle_key: Option<&str>,
        style: &RlmStyle,
    ) -> Vec<String> {
        let runs = build_dag_runs(snapshot);
        let inner = width.saturating_sub(1).max(1);
        let rendered = render_rlm_pane(
            &runs,
            &self.state,
            inner,
            height.max(1),
            &DagRenderOptions {
                style: style.clone(),
                now: snapshot.now,
                hints: rlm_pane_hints(keys),
                focused: Some(focused),
                focus_hint: toggle_key.map(|k| format!("{k} {}", if focused { "chat" } else { "focus" })),
                error: snapshot.error.clone(),
                task_count: snapshot.tasks.len(),
            },
        );
        self.state = rendered.state;
        rendered.lines.into_iter().map(|l| format!(" {l}")).collect()
    }
}
