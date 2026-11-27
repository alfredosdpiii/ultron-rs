//! The RLM run as a live graph (experimental/rlm-graph.ts): the root turn, its `rlm` cells and everything they
//! started (typed agent tasks, `rlm.spawn` children, `rlm.infer` frames, `rlm.map` fan-outs, workflows, shell jobs,
//! tool calls), laid out with box-drawing connectors, and the three levels the TUI shows: a one-line footer summary,
//! the docked panel, and the focus view's body (see [`super::focus`]).

mod build;
mod render;

use std::collections::HashMap;

use jsrt::number;
use serde_json::Value;
use theme::Fg;

use super::flat;
use super::visualizer::{RlmStyle, format_duration};

pub use build::build_rlm_graph;
pub use render::{
    budget_gauges, render_details, render_gauges, render_graph_row, render_kernel_strip, render_rlm_dock,
    render_rlm_footer,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GraphStatus {
    Running,
    Pending,
    Done,
    Failed,
    Cancelled,
    Incomplete,
}

impl GraphStatus {
    pub fn name(self) -> &'static str {
        match self {
            GraphStatus::Running => "running",
            GraphStatus::Pending => "pending",
            GraphStatus::Done => "done",
            GraphStatus::Failed => "failed",
            GraphStatus::Cancelled => "cancelled",
            GraphStatus::Incomplete => "incomplete",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GraphKind {
    Turn,
    Cell,
    Task,
    Child,
    Background,
    Infer,
    Fanout,
    Job,
    Tool,
    Workflow,
    Earlier,
    /// The RLM pane's "N tool calls" node.
    Tools,
}

const GRAPH_KIND_NAMES: [&str; 12] = [
    "turn",
    "cell",
    "task",
    "child",
    "background",
    "infer",
    "fanout",
    "job",
    "tool",
    "workflow",
    "earlier",
    "tools",
];

impl GraphKind {
    pub fn name(self) -> &'static str {
        GRAPH_KIND_NAMES[self as usize]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GraphProgress {
    pub total: usize,
    pub done: usize,
    pub running: usize,
    pub pending: usize,
    pub incomplete: usize,
    pub failed: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphNode {
    /// Stable across refreshes (selection and collapse state are keyed by it).
    pub key: String,
    pub kind: GraphKind,
    pub label: String,
    pub status: GraphStatus,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub tokens: Option<f64>,
    pub cost: Option<f64>,
    pub progress: Option<GraphProgress>,
    /// Workflow join marker, e.g. "⇐ plan+code (all)".
    pub join: Option<String>,
    pub note: Option<String>,
    pub children: Vec<GraphNode>,
    pub details: Vec<(String, String)>,
    /// Frame trace id to fetch for details (`rlm.frames {id}`).
    pub trace_id: Option<String>,
}

impl GraphNode {
    pub(crate) fn new(key: String, kind: GraphKind, label: String, status: GraphStatus) -> GraphNode {
        GraphNode {
            key,
            kind,
            label,
            status,
            started_at: None,
            ended_at: None,
            tokens: None,
            cost: None,
            progress: None,
            join: None,
            note: None,
            children: Vec::new(),
            details: Vec::new(),
            trace_id: None,
        }
    }
}

pub(crate) fn d(label: &str, value: impl Into<String>) -> (String, String) {
    (label.to_string(), value.into())
}

pub fn is_finished(status: GraphStatus) -> bool {
    !matches!(status, GraphStatus::Running | GraphStatus::Pending)
}

pub fn task_status(state: &str) -> GraphStatus {
    match state {
        "running" => GraphStatus::Running,
        "admitted" => GraphStatus::Pending,
        "completed" => GraphStatus::Done,
        "cancelled" => GraphStatus::Cancelled,
        "interrupted" => GraphStatus::Incomplete,
        _ => GraphStatus::Failed,
    }
}

/// Combine children statuses into a parent status.
pub fn rollup(c: &GraphProgress) -> GraphStatus {
    if c.running > 0 {
        return GraphStatus::Running;
    }
    if c.pending > 0 {
        return if c.done + c.failed + c.incomplete > 0 {
            GraphStatus::Running
        } else {
            GraphStatus::Pending
        };
    }
    if c.failed > 0 && c.done == 0 && c.incomplete == 0 {
        return GraphStatus::Failed;
    }
    if c.failed + c.incomplete > 0 {
        return GraphStatus::Incomplete;
    }
    GraphStatus::Done
}

pub fn count(statuses: &[GraphStatus], total: usize) -> GraphProgress {
    let mut p = GraphProgress {
        total,
        ..GraphProgress::default()
    };
    for s in statuses {
        match s {
            GraphStatus::Done => p.done += 1,
            GraphStatus::Running => p.running += 1,
            GraphStatus::Pending => p.pending += 1,
            GraphStatus::Incomplete => p.incomplete += 1,
            _ => p.failed += 1,
        }
    }
    p.pending += total.saturating_sub(statuses.len());
    p
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphRow<'a> {
    pub node: &'a GraphNode,
    pub depth: usize,
    /// For each ancestor level below the root: whether that ancestor was the last child (no rail).
    pub rails: Vec<bool>,
    pub last: bool,
    pub collapsed: bool,
    /// Descendants hidden by collapse.
    pub hidden: usize,
    pub parent_key: Option<String>,
}

/// Collapse overrides by node key: true collapsed, false expanded; absent means automatic.
pub type CollapseState = HashMap<String, bool>;

/// Finished subtrees fold, unless something under them still runs.
pub fn default_collapsed(node: &GraphNode) -> bool {
    if node.children.is_empty() || node.kind == GraphKind::Turn {
        return false;
    }
    if node.kind == GraphKind::Earlier {
        return true;
    }
    is_finished(node.status) && !has_live_descendant(node)
}

fn has_live_descendant(node: &GraphNode) -> bool {
    node.children
        .iter()
        .any(|c| !is_finished(c.status) || has_live_descendant(c))
}

fn descendants(node: &GraphNode) -> usize {
    node.children.iter().map(|c| 1 + descendants(c)).sum()
}

fn visit_layout<'a>(
    node: &'a GraphNode,
    depth: usize,
    rails: Vec<bool>,
    last: bool,
    parent: Option<&str>,
    collapse: &CollapseState,
    rows: &mut Vec<GraphRow<'a>>,
) {
    let collapsed = !node.children.is_empty()
        && collapse
            .get(&node.key)
            .copied()
            .unwrap_or_else(|| default_collapsed(node));
    rows.push(GraphRow {
        node,
        depth,
        rails: rails.clone(),
        last,
        collapsed,
        hidden: if collapsed { descendants(node) } else { 0 },
        parent_key: parent.map(str::to_string),
    });
    if collapsed {
        return;
    }
    let n = node.children.len();
    for (i, child) in node.children.iter().enumerate() {
        let child_rails = if depth == 0 {
            Vec::new()
        } else {
            let mut r = rails.clone();
            r.push(last);
            r
        };
        visit_layout(
            child,
            depth + 1,
            child_rails,
            i == n - 1,
            Some(&node.key),
            collapse,
            rows,
        );
    }
}

pub fn layout_graph<'a>(root: &'a GraphNode, collapse: &CollapseState) -> Vec<GraphRow<'a>> {
    let mut rows = Vec::new();
    visit_layout(root, 0, Vec::new(), true, None, collapse, &mut rows);
    rows
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner_glyph(frame: i64) -> &'static str {
    SPINNER[(frame.unsigned_abs() % 10) as usize]
}

pub fn status_glyph(status: GraphStatus, spinner: &str) -> String {
    match status {
        GraphStatus::Running => spinner.to_string(),
        GraphStatus::Pending => "○".into(),
        GraphStatus::Done => "✓".into(),
        GraphStatus::Failed => "✗".into(),
        GraphStatus::Cancelled => "⊘".into(),
        GraphStatus::Incomplete => "◐".into(),
    }
}

pub fn status_color(status: GraphStatus) -> Fg {
    match status {
        GraphStatus::Running => Fg::Accent,
        GraphStatus::Pending => Fg::Muted,
        GraphStatus::Done => Fg::Success,
        GraphStatus::Failed => Fg::Error,
        GraphStatus::Cancelled => Fg::Dim,
        GraphStatus::Incomplete => Fg::Warning,
    }
}

pub fn format_tokens(tokens: f64) -> String {
    if tokens < 1000.0 {
        number::to_string(tokens.round())
    } else if tokens < 10_000.0 {
        format!("{}k", number::to_fixed(tokens / 1000.0, 1))
    } else if tokens < 1_000_000.0 {
        format!("{}k", number::to_string((tokens / 1000.0).round()))
    } else {
        format!("{}M", number::to_fixed(tokens / 1_000_000.0, 1))
    }
}

pub fn format_cost(cost: f64) -> String {
    if cost >= 0.01 {
        format!("${}", number::to_fixed(cost, 2))
    } else {
        format!("${}", number::to_fixed(cost, 3))
    }
}

pub fn format_bytes(bytes: f64) -> String {
    let mib = bytes / (1024.0 * 1024.0);
    if mib >= 1024.0 {
        format!("{} GiB", number::to_fixed(mib / 1024.0, 1))
    } else {
        format!("{} MiB", number::to_string(mib.round()))
    }
}

/// `▰▰▰▱▱` filled in proportion; at least one cell shows any nonzero progress.
pub fn bar(fraction: f64, cells: usize) -> String {
    let clamped = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut filled = (clamped * cells as f64).round() as usize;
    if filled == 0 && clamped > 0.0 {
        filled = 1;
    }
    if filled == cells && clamped < 1.0 {
        filled = cells.saturating_sub(1);
    }
    format!("{}{}", "▰".repeat(filled), "▱".repeat(cells.saturating_sub(filled)))
}

const MAX_PLAUSIBLE_MS: f64 = 7.0 * 24.0 * 60.0 * 60_000.0;

/// Elapsed time of a node (running: until `now`).
pub fn elapsed(started_at: Option<f64>, ended_at: Option<f64>, status: GraphStatus, now: f64) -> Option<String> {
    let start = started_at?;
    let end = match ended_at {
        Some(e) => e,
        None if is_finished(status) => return None,
        None => now,
    };
    let duration = (end - start).max(0.0);
    (duration <= MAX_PLAUSIBLE_MS).then(|| format_duration(duration))
}

pub(crate) fn node_elapsed(node: &GraphNode, now: f64) -> Option<String> {
    elapsed(node.started_at, node.ended_at, node.status, now)
}

/// How rows are drawn.
#[derive(Clone)]
pub struct RowRenderOptions {
    pub style: RlmStyle,
    pub now: f64,
    pub spinner: String,
    /// Key of the selected node (focus mode): drawn with a marker.
    pub selected_key: Option<String>,
    /// Reserve a two-column selection gutter (focus mode).
    pub gutter: bool,
}

impl RowRenderOptions {
    pub fn new(style: RlmStyle, now: f64, spinner: &str) -> RowRenderOptions {
        RowRenderOptions {
            style,
            now,
            spinner: spinner.to_string(),
            selected_key: None,
            gutter: false,
        }
    }
}

/// The rows to show when there are more than `max`: a window keeping `focus` in view.
pub fn window_rows(total: usize, max: usize, focus: usize) -> (usize, usize) {
    if total <= max {
        return (0, total);
    }
    let size = max.max(1);
    let clamped = focus.min(total - 1);
    let mut start = (clamped as isize - size as isize + 2).max(0) as usize;
    if clamped < size - 1 {
        start = 0;
    }
    start = start.min(total - size);
    (start, start + size)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Gauge {
    pub label: String,
    pub used: f64,
    pub limit: Option<f64>,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphTotals {
    pub running: usize,
    pub tasks: usize,
    pub failed: usize,
    pub jobs_running: usize,
    pub tools_running: usize,
    pub fanout_keys: Vec<String>,
    pub cell_key: Option<String>,
    pub nodes: usize,
}

fn record_node_kind_totals(node: &GraphNode, t: &mut GraphTotals) {
    match node.kind {
        GraphKind::Task | GraphKind::Child | GraphKind::Background => t.tasks += 1,
        GraphKind::Job if node.status == GraphStatus::Running => t.jobs_running += 1,
        GraphKind::Tool if node.status == GraphStatus::Running => t.tools_running += 1,
        GraphKind::Fanout => t.fanout_keys.push(node.key.clone()),
        _ => {}
    }
}

fn record_node_totals(node: &GraphNode, t: &mut GraphTotals) {
    t.nodes += 1;
    let active = matches!(node.status, GraphStatus::Running | GraphStatus::Pending);
    if !matches!(node.kind, GraphKind::Turn | GraphKind::Earlier) && active {
        t.running += 1;
    }
    if node.status == GraphStatus::Failed && node.kind != GraphKind::Earlier {
        t.failed += 1;
    }
    record_node_kind_totals(node, t);
}

fn visit_totals(node: &GraphNode, t: &mut GraphTotals) {
    record_node_totals(node, t);
    if node.kind != GraphKind::Earlier {
        for c in &node.children {
            visit_totals(c, t);
        }
    }
}

pub fn graph_totals(root: &GraphNode) -> GraphTotals {
    let mut t = GraphTotals::default();
    visit_totals(root, &mut t);
    let cells: Vec<&GraphNode> = root.children.iter().filter(|c| c.kind == GraphKind::Cell).collect();
    t.cell_key = cells
        .iter()
        .rev()
        .find(|c| c.status == GraphStatus::Running)
        .or(cells.last())
        .map(|c| c.key.clone());
    t
}

pub(crate) fn find_node<'a>(root: &'a GraphNode, key: &str) -> Option<&'a GraphNode> {
    if root.key == key {
        return Some(root);
    }
    root.children.iter().find_map(|c| find_node(c, key))
}

pub(crate) fn count_active_tasks(root: &GraphNode) -> usize {
    let mut active = 0;
    fn visit(node: &GraphNode, active: &mut usize) {
        if matches!(node.kind, GraphKind::Task | GraphKind::Child | GraphKind::Background) && !is_finished(node.status)
        {
            *active += 1;
        }
        if node.kind != GraphKind::Earlier {
            for c in &node.children {
                visit(c, active);
            }
        }
    }
    visit(root, &mut active);
    active
}

pub fn graph_active(root: &GraphNode) -> bool {
    root.status == GraphStatus::Running || graph_totals(root).running > 0
}

/// How the views draw.
#[derive(Clone)]
pub struct RlmViewOptions {
    pub style: RlmStyle,
    pub spinner_frame: i64,
    /// Key hint for the focus view, e.g. "alt+g".
    pub focus_key: Option<String>,
    pub collapse: CollapseState,
}

impl Default for RlmViewOptions {
    fn default() -> Self {
        RlmViewOptions {
            style: RlmStyle::plain(),
            spinner_frame: 0,
            focus_key: None,
            collapse: CollapseState::new(),
        }
    }
}

fn format_remaining_value(v: &Value, fallback: &str) -> String {
    match v {
        Value::Null => fallback.to_string(),
        Value::Number(n) => number::to_string(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Frame trace (from `rlm.frames {id}`) as detail lines.
pub fn frame_trace_details(trace: &Value) -> Vec<(String, String)> {
    if !trace.is_object() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for (field, limit) in [
        ("model", None),
        ("detail", Some(300)),
        ("value", Some(400)),
        ("error", Some(400)),
    ] {
        if let Some(text) = trace[field].as_str() {
            let value = match limit {
                Some(n) => flat(text, n),
                None => text.to_string(),
            };
            lines.push(d(field, value));
        }
    }
    if let Some(v) = trace["views"].as_array() {
        lines.push(d(
            "views",
            format!("{} context view{}", v.len(), if v.len() == 1 { "" } else { "s" }),
        ));
    }
    if let Some(a) = trace["attempts"].as_array() {
        lines.push(d("attempts", a.len().to_string()));
    }
    let r = &trace["remaining"];
    if r.is_object() {
        lines.push(d(
            "remaining",
            format!(
                "calls {} · tokens {} · depth {}",
                format_remaining_value(&r["calls"], "∞"),
                format_remaining_value(&r["tokens"], "∞"),
                format_remaining_value(&r["depth"], "?")
            ),
        ));
    }
    lines
}
