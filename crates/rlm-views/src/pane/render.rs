use std::collections::HashMap;

use jsrt::number;
use theme::Fg;
use tui::{truncate_to_width, visible_width};

use crate::graph::{GraphKind, GraphProgress, GraphStatus, bar, status_color};
use crate::visualizer::{RlmStyle, format_duration};

use super::build::is_active;
use super::{
    DagNode, DagRun, RlmPaneState, RunCounts, RunKind, current_run, node_order, run_counts, selected_node, summarized,
};

fn pad_to(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cut_text = cut(text, width);
    let w = visible_width(&cut_text);
    format!("{cut_text}{}", " ".repeat(width.saturating_sub(w)))
}

fn cut(text: &str, width: usize) -> String {
    if visible_width(text) > width {
        truncate_to_width(text, width, "…", false)
    } else {
        text.to_string()
    }
}

fn elapsed_of(node: &DagNode, now: f64) -> Option<String> {
    crate::graph::elapsed(node.started_at, node.ended_at, node.status(), now)
}

pub fn status_word(node: &DagNode) -> &'static str {
    match node.status() {
        GraphStatus::Running => "● Running",
        GraphStatus::Pending => "○ Pending",
        GraphStatus::Done => "✓ Done",
        GraphStatus::Failed => "✗ Failed",
        GraphStatus::Cancelled => {
            if node.note.as_deref().is_some_and(|n| n.starts_with("skipped")) {
                "⊘ Skipped"
            } else {
                "⊘ Cancelled"
            }
        }
        GraphStatus::Incomplete => "◐ Incomplete",
    }
}

fn dot(status: GraphStatus) -> &'static str {
    match status {
        GraphStatus::Running => "●",
        GraphStatus::Pending => "○",
        GraphStatus::Done => "✓",
        GraphStatus::Failed => "✗",
        GraphStatus::Cancelled => "⊘",
        GraphStatus::Incomplete => "◐",
    }
}

fn tone(status: GraphStatus) -> Fg {
    if status == GraphStatus::Pending {
        Fg::Dim
    } else {
        status_color(status)
    }
}

fn progress_line(p: &GraphProgress, width: usize) -> String {
    let finished = p.done + p.failed + p.incomplete;
    let text = format!("{finished}/{}", p.total);
    let cells = (width as isize - visible_width(&text) as isize - 1).clamp(3, 10) as usize;
    format!("{} {text}", bar(finished as f64 / p.total.max(1) as f64, cells))
}

const GAP: usize = 2;
const MIN_BOX: usize = 20;
const MAX_BOX: usize = 30;
const MAX_SINGLE_BOX: usize = 48;

/// Boxes per row and the box width for a wave of `count` nodes in `width` columns.
pub fn wave_geometry(count: usize, width: usize, natural: usize) -> (usize, usize) {
    let room = width.max(1);
    let per_row = count.min((room + GAP) / (MIN_BOX + GAP)).max(1);
    let max = if count == 1 {
        MAX_BOX.max(MAX_SINGLE_BOX.min(natural))
    } else {
        MAX_BOX
    };
    let box_width = max
        .min((room as isize - (GAP * (per_row - 1)) as isize).max(0) as usize / per_row)
        .max(1);
    (per_row, box_width)
}

fn names(run: &DagRun) -> HashMap<&str, &str> {
    run.nodes.iter().map(|n| (n.key.as_str(), n.name.as_str())).collect()
}

fn sub_of(node: &DagNode, run: &DagRun) -> String {
    let names = names(run);
    if node.depends_on.is_empty() {
        node.origin.clone()
    } else {
        format!(
            "← {}",
            node.depends_on
                .iter()
                .map(|k| names.get(k.as_str()).copied().unwrap_or(k))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn natural_box_width(node: &DagNode, run: &DagRun) -> usize {
    visible_width(&format!("> [-] {}", node.name))
        .max(visible_width(&sub_of(node, run)))
        .max(16)
        + 4
}

fn draw_box(lines: &[String], width: usize, border: Fg, style: &RlmStyle, bold: bool) -> Vec<String> {
    if width < 5 {
        return lines.iter().map(|l| pad_to(l, width)).collect();
    }
    let inner = width - 4;
    let edge = |t: &str| {
        if bold {
            style.bold(&style.fg(border, t))
        } else {
            style.fg(border, t)
        }
    };
    let mut out = vec![edge(&format!("┌{}┐", "─".repeat(width - 2)))];
    for l in lines {
        out.push(format!("{} {} {}", edge("│"), pad_to(l, inner), edge("│")));
    }
    out.push(edge(&format!("└{}┘", "─".repeat(width - 2))));
    out
}

fn node_status_line(node: &DagNode, inner: usize, color: Fg, style: &RlmStyle, now: f64) -> String {
    let word = status_word(node);
    match elapsed_of(node, now) {
        Some(t) if visible_width(word) + 2 + visible_width(&t) <= inner => {
            format!("{}  {}", style.fg(color, word), style.fg(Fg::Dim, &t))
        }
        _ => style.fg(color, word),
    }
}

fn node_sub_line(node: &DagNode, run: &DagRun, inner: usize, style: &RlmStyle) -> String {
    match &node.progress {
        Some(p) if matches!(node.kind(), GraphKind::Fanout | GraphKind::Workflow | GraphKind::Tools) => {
            style.fg(tone(node.status()), &progress_line(p, inner))
        }
        _ if !node.depends_on.is_empty() => style.fg(Fg::Muted, &sub_of(node, run)),
        _ => style.fg(Fg::Dim, &node.origin),
    }
}

fn node_box(
    node: &DagNode,
    run: &DagRun,
    width: usize,
    selected: bool,
    folded: bool,
    style: &RlmStyle,
    now: f64,
) -> Vec<String> {
    let inner = width.saturating_sub(4).max(1);
    let marker = if selected {
        style.fg(Fg::Accent, ">")
    } else {
        " ".into()
    };
    let fold = style.fg(Fg::Dim, if folded { "[+]" } else { "[-]" });
    let name = if selected {
        style.bold(&node.name)
    } else {
        node.name.clone()
    };
    let color = tone(node.status());
    let border = if selected { Fg::Accent } else { Fg::Dim };
    if folded {
        let glyph = style.fg(color, dot(node.status()));
        let head = cut(&format!("{marker} {fold} {name}"), inner.saturating_sub(2).max(1));
        return draw_box(&[format!("{head} {glyph}")], width, border, style, selected);
    }
    let status = node_status_line(node, inner, color, style, now);
    let sub = node_sub_line(node, run, inner, style);
    draw_box(
        &[format!("{marker} {fold} {name}"), status, sub],
        width,
        border,
        style,
        selected,
    )
}

fn card_body_line(node: &DagNode, width: usize, style: &RlmStyle) -> String {
    if node.status() == GraphStatus::Failed
        && let Some(note) = node.note.as_deref()
    {
        return style.fg(Fg::Error, note);
    }
    if let Some(t) = &node.text {
        return style.fg(Fg::Text, t);
    }
    if let Some(n) = &node.note {
        return style.fg(Fg::Muted, &format!("→ {n}"));
    }
    if node.status() == GraphStatus::Pending {
        let suffix = if node.depends_on.is_empty() {
            ""
        } else {
            " for dependencies"
        };
        return style.fg(Fg::Dim, &format!("waiting{suffix}"));
    }
    if let Some(p) = &node.progress {
        return style.fg(Fg::Muted, &progress_line(p, width.saturating_sub(4)));
    }
    style.fg(Fg::Dim, "no output yet")
}

fn card_stats_line(node: &DagNode, run: &DagRun, style: &RlmStyle, now: f64) -> String {
    let mut stats = Vec::new();
    if node.status() == GraphStatus::Pending {
        let waited = run.started_at.filter(|s| now >= *s).map(|s| format_duration(now - s));
        stats.push(waited.map_or("waiting".to_string(), |w| format!("waiting {w}")));
    } else if let Some(t) = elapsed_of(node, now) {
        stats.push(t);
    }
    if let Some(t) = node.turns {
        stats.push(format!(
            "{} turn{}",
            number::to_string(t),
            if t == 1.0 { "" } else { "s" }
        ));
    }
    if let Some(t) = node.tool_calls {
        stats.push(format!(
            "{} tool{}",
            number::to_string(t),
            if t == 1.0 { "" } else { "s" }
        ));
    }
    let joined = if stats.is_empty() {
        "—".to_string()
    } else {
        stats.join(" · ")
    };
    style.fg(Fg::Dim, &joined)
}

fn card_lines(node: &DagNode, run: &DagRun, width: usize, selected: bool, style: &RlmStyle, now: f64) -> Vec<String> {
    let color = tone(node.status());
    let marker = if selected {
        format!("{} ", style.fg(Fg::Accent, ">"))
    } else {
        String::new()
    };
    let head = format!(
        "{marker}{} {}",
        style.fg(color, dot(node.status())),
        style.fg(
            if selected { Fg::Accent } else { Fg::Text },
            node.prompt.as_deref().unwrap_or(&node.name)
        )
    );
    let meta = match &node.model {
        None => node.category.clone(),
        Some(m) => format!("{} · {m}", node.category),
    };
    let lines = vec![
        head,
        style.fg(Fg::Muted, &format!("{} · {meta}", node.name)),
        card_body_line(node, width, style),
        card_stats_line(node, run, style, now),
    ];
    draw_box(
        &lines,
        width,
        if selected { Fg::Accent } else { Fg::Dim },
        style,
        selected,
    )
}

/// Pack hint items into lines of `width`, two spaces apart.
pub fn pack_hints(items: &[String], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for item in items {
        let candidate = if current.is_empty() {
            item.clone()
        } else {
            format!("{current}  {item}")
        };
        if visible_width(&candidate) <= width || current.is_empty() {
            current = candidate;
        } else {
            lines.push(current);
            current = item.clone();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines.iter().map(|l| cut(l, width)).collect()
}

/// The body lines and each node box's first and last body line.
pub struct Body {
    pub lines: Vec<String>,
    pub spans: HashMap<String, (usize, usize)>,
}

fn center_line(text: &str, bound: usize) -> String {
    let pad = (bound as isize - visible_width(text) as isize).max(0) as usize / 2;
    cut(&format!("{}{text}", " ".repeat(pad)), bound)
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal wave row layout helper; parameters match distinct caller state"
)]
fn append_wave_rows(
    wave: &[String],
    run: &DagRun,
    by_key: &HashMap<&str, &DagNode>,
    selected: Option<&String>,
    state: &RlmPaneState,
    bound: usize,
    style: &RlmStyle,
    now: f64,
    lines: &mut Vec<String>,
    spans: &mut HashMap<String, (usize, usize)>,
) {
    let lone = if wave.len() == 1 {
        natural_box_width(by_key[wave[0].as_str()], run)
    } else {
        0
    };
    let (per_row, box_width) = wave_geometry(wave.len(), bound, lone);
    let mut start = 0;
    while start < wave.len() {
        if start > 0 {
            lines.push(String::new());
        }
        let row: Vec<&DagNode> = wave[start..(start + per_row).min(wave.len())]
            .iter()
            .map(|k| by_key[k.as_str()])
            .collect();
        let boxes: Vec<Vec<String>> = row
            .iter()
            .map(|n| {
                node_box(
                    n,
                    run,
                    box_width,
                    Some(&n.key) == selected,
                    state.folded.contains(&n.key),
                    style,
                    now,
                )
            })
            .collect();
        let height = boxes.iter().map(Vec::len).max().unwrap_or(0);
        let row_width = row.len() * box_width + GAP * (row.len() - 1);
        let indent = " ".repeat((bound as isize - row_width as isize).max(0) as usize / 2);
        let top = lines.len();
        for line in 0..height {
            let joined = boxes
                .iter()
                .map(|b| b.get(line).cloned().unwrap_or_else(|| " ".repeat(box_width)))
                .collect::<Vec<_>>()
                .join(&" ".repeat(GAP));
            lines.push(cut(&format!("{indent}{joined}"), bound));
        }
        for (i, n) in row.iter().enumerate() {
            spans.insert(n.key.clone(), (top, top + boxes[i].len() - 1));
        }
        start += per_row;
    }
}

fn append_dependencies(
    run: &DagRun,
    by_key: &HashMap<&str, &DagNode>,
    bound: usize,
    style: &RlmStyle,
    lines: &mut Vec<String>,
) {
    lines.push(String::new());
    lines.push(style.bold(&style.fg(Fg::Text, "Dependencies")));
    if run.edges.is_empty() {
        let none = if run.kind == RunKind::Turn {
            "  none · each cell's work is one frontier"
        } else {
            "  none"
        };
        lines.push(style.fg(Fg::Dim, &cut(none, bound)));
    }
    const EDGE_LIMIT: usize = 40;
    let name = |k: &str| by_key.get(k).map_or(k.to_string(), |n| n.name.clone());
    for (from, to) in run.edges.iter().take(EDGE_LIMIT) {
        lines.push(cut(
            &format!(
                "  {} {} {}",
                style.fg(Fg::Text, &name(from)),
                style.fg(Fg::Dim, "→"),
                style.fg(Fg::Text, &name(to))
            ),
            bound,
        ));
    }
    if run.edges.len() > EDGE_LIMIT {
        lines.push(style.fg(
            Fg::Dim,
            &cut(&format!("  … {} more", run.edges.len() - EDGE_LIMIT), bound),
        ));
    }
}

/// The scrolled body of the pane: waves of boxes with arrows, the dependencies, and the node cards.
pub fn render_dag_body(run: &DagRun, state: &RlmPaneState, width: usize, style: &RlmStyle, now: f64) -> Body {
    let bound = width.max(1);
    let selected = selected_node(run, state);
    let by_key: HashMap<&str, &DagNode> = run.nodes.iter().map(|n| (n.key.as_str(), n)).collect();
    let mut lines: Vec<String> = Vec::new();
    let mut spans = HashMap::new();
    for (wave_index, wave) in run.waves.iter().enumerate() {
        append_wave_rows(
            wave,
            run,
            &by_key,
            selected.as_ref(),
            state,
            bound,
            style,
            now,
            &mut lines,
            &mut spans,
        );
        if wave.len() > 1 {
            lines.push(cut(&style.fg(Fg::Dim, "  · Same frontier"), bound));
        }
        if wave_index < run.waves.len() - 1 {
            lines.push(center_line(&style.fg(Fg::Dim, "│"), bound));
            lines.push(center_line(&style.fg(Fg::Accent, "▼"), bound));
        }
    }
    append_dependencies(run, &by_key, bound, style, &mut lines);
    if state.details {
        lines.push(String::new());
        lines.push(style.bold(&style.fg(Fg::Text, "Node details")));
        for key in node_order(run) {
            let node = by_key[key.as_str()];
            for l in card_lines(node, run, bound, Some(&key) == selected.as_ref(), style, now) {
                lines.push(cut(&l, bound));
            }
        }
    }
    Body { lines, spans }
}

/// How the pane draws.
#[derive(Clone)]
pub struct DagRenderOptions {
    pub style: RlmStyle,
    pub now: f64,
    /// Key hints for the footer, e.g. "Tab/n next".
    pub hints: Vec<String>,
    /// Whether the pane has the input focus (`None`: unknown, drawn as focused).
    pub focused: Option<bool>,
    /// Shown as the first hint, e.g. "alt+w chat".
    pub focus_hint: Option<String>,
    /// Inspection failure: the footer shows it instead of "Connected".
    pub error: Option<String>,
    /// Tasks listed by the host, for the breadcrumb.
    pub task_count: usize,
}

impl DagRenderOptions {
    pub fn new(now: f64) -> DagRenderOptions {
        DagRenderOptions {
            style: RlmStyle::plain(),
            now,
            hints: Vec::new(),
            focused: None,
            focus_hint: None,
            error: None,
            task_count: 0,
        }
    }
}

/// What [`render_rlm_pane`] drew.
pub struct PaneRender {
    pub lines: Vec<String>,
    pub state: RlmPaneState,
    pub run_key: Option<String>,
}

fn run_status_badge(c: &RunCounts) -> (&'static str, Fg) {
    if c.running > 0 {
        ("Running", Fg::Accent)
    } else if c.pending > 0 {
        ("Pending", if c.failed > 0 { Fg::Error } else { Fg::Dim })
    } else if c.failed > 0 {
        ("Failed", Fg::Error)
    } else {
        ("Finished", Fg::Success)
    }
}

fn pane_header(runs: &[DagRun], run: Option<&DagRun>, bound: usize, o: &DagRenderOptions) -> Vec<String> {
    let style = &o.style;
    let Some(run) = run else {
        return vec![
            style.bold(&style.fg(Fg::Accent, "RLM")),
            style.fg(Fg::Muted, "RLM pane"),
        ];
    };
    let c = run_counts(run);
    let index = runs.iter().position(|r| r.key == run.key).unwrap_or(0) + 1;
    let counter = format!("{index}/{}", runs.len());
    let title = cut(&run.title, bound.saturating_sub(visible_width(&counter) + 2).max(1));
    let (word, color) = run_status_badge(&c);
    let mut extra = vec![format!("Done {}/{}", c.done, c.total)];
    if c.running > 0 {
        extra.push(format!("{} running", c.running));
    }
    if c.failed > 0 {
        extra.push(format!("{} failed", c.failed));
    }
    extra.push(format!("wave {}/{}", c.wave, c.waves));
    vec![
        format!(
            "{} {} {}",
            style.bold(&style.fg(Fg::Accent, "RLM")),
            style.fg(Fg::Dim, "·"),
            style.fg(Fg::Muted, &run.id)
        ),
        format!(
            "{}  {}",
            style.fg(
                Fg::Muted,
                &format!(
                    "RLM pane / {}",
                    if run.kind == RunKind::Turn { "turn" } else { "workflow" }
                )
            ),
            style.fg(Fg::Dim, &format!("Tasks ({})", o.task_count))
        ),
        format!(
            "{}  {}",
            style.bold(&style.fg(Fg::Text, &title)),
            style.fg(Fg::Dim, &counter)
        ),
        format!(
            "{} {} {}",
            style.fg(color, word),
            style.fg(Fg::Dim, "·"),
            style.fg(Fg::Muted, &extra.join(" · "))
        ),
    ]
}

fn pane_body(run: Option<&DagRun>, state: &RlmPaneState, bound: usize, o: &DagRenderOptions) -> Body {
    let style = &o.style;
    match run {
        None => Body {
            lines: vec![
                String::new(),
                style.fg(Fg::Dim, "No RLM work in this turn yet."),
                style.fg(Fg::Dim, "Spawned children and workflows.run nodes"),
                style.fg(Fg::Dim, "appear here as dependency waves."),
            ],
            spans: HashMap::new(),
        },
        Some(run) => render_dag_body(run, state, bound, style, o.now),
    }
}

fn pane_footer_chrome(
    header_len: usize,
    rows: usize,
    bound: usize,
    o: &DagRenderOptions,
) -> (Vec<String>, Vec<String>, Vec<String>, String) {
    let style = &o.style;
    let mut hints: Vec<String> = o.focus_hint.iter().cloned().collect();
    hints.extend(o.hints.iter().cloned());
    let connection = match &o.error {
        Some(e) => style.fg(
            Fg::Error,
            &format!(
                "● {}",
                cut(&format!("Inspection failed: {e}"), bound.saturating_sub(14).max(1))
            ),
        ),
        None => style.fg(Fg::Success, "● Connected"),
    };
    let hint_fg = if o.focused == Some(false) { Fg::Dim } else { Fg::Muted };
    let mut footer_hints: Vec<String> = pack_hints(&hints, bound).iter().map(|l| style.fg(hint_fg, l)).collect();
    let mut separator = vec![style.fg(Fg::Dim, &"─".repeat(bound))];
    let mut blank = vec![String::new()];
    if rows < header_len + 1 + 2 + footer_hints.len() + 6 {
        blank.clear();
        separator.clear();
        footer_hints = if footer_hints.is_empty() {
            Vec::new()
        } else {
            vec![cut(&footer_hints.join("  "), bound)]
        };
    }
    (blank, separator, footer_hints, connection)
}

fn compute_pane_scroll(body: &Body, state: &RlmPaneState, selected: Option<&String>, room: usize) -> usize {
    let max_scroll = body.lines.len().saturating_sub(room);
    let mut scroll = state.scroll.min(max_scroll);
    if !state.reveal {
        return scroll;
    }
    if let Some((top, bottom)) = selected.and_then(|sel| body.spans.get(sel)) {
        if *top < scroll {
            scroll = *top;
        } else if *bottom >= scroll + room {
            scroll = (*top).min(bottom + 1 - room);
        }
    }
    scroll.min(max_scroll)
}

/// Render the pane into exactly `height` lines of at most `width` columns: a fixed header, the scrolled body and a
/// fixed footer. Returns the state with the scroll it used.
pub fn render_rlm_pane(
    runs: &[DagRun],
    state: &RlmPaneState,
    width: usize,
    height: usize,
    o: &DagRenderOptions,
) -> PaneRender {
    let style = &o.style;
    let bound = width.max(1);
    let rows = height.max(1);
    let run = current_run(runs, state);
    let header = pane_header(runs, run, bound, o);
    let body = pane_body(run, state, bound, o);
    let (blank, separator, footer_hints, connection) = pane_footer_chrome(header.len(), rows, bound, o);
    let fixed = header.len() + blank.len() + separator.len() + 1 + footer_hints.len();
    let room = (rows as isize - fixed as isize).max(1) as usize;
    let selected = run.and_then(|r| selected_node(r, state));
    let scroll = compute_pane_scroll(&body, state, selected.as_ref(), room);
    let total = body.lines.len();
    let position = if total == 0 {
        "0/0".to_string()
    } else {
        format!("{}-{}/{total}", scroll + 1, total.min(scroll + room))
    };
    let status = cut(&format!("{connection}  {}", style.fg(Fg::Dim, &position)), bound);
    let mut lines: Vec<String> = header;
    lines.extend(blank);
    lines.extend(body.lines.iter().skip(scroll).take(room).cloned());
    let footer_len = separator.len() + 1 + footer_hints.len();
    while lines.len() < rows.saturating_sub(footer_len) {
        lines.push(String::new());
    }
    lines.extend(separator);
    lines.push(status);
    lines.extend(footer_hints);
    let mut out: Vec<String> = lines.iter().take(rows).map(|l| cut(l, bound)).collect();
    while out.len() < rows {
        out.push(String::new());
    }
    let mut next = state.clone();
    next.scroll = scroll;
    next.reveal = false;
    PaneRender {
        lines: out,
        state: next,
        run_key: run.map(|r| r.key.clone()),
    }
}

fn summary_rank(status: GraphStatus) -> u8 {
    match status {
        GraphStatus::Running => 0,
        GraphStatus::Pending => 1,
        GraphStatus::Failed | GraphStatus::Incomplete => 2,
        _ => 3,
    }
}

fn wave_node_tail_and_said(node: &DagNode, run: &DagRun, now: f64) -> (String, Option<String>) {
    if node.status() == GraphStatus::Pending {
        let waited = run
            .started_at
            .filter(|s| now >= *s)
            .map_or_default(|s| format!(" {}", format_duration(now - s)));
        return (format!(" · waiting{waited}"), None);
    }
    let said = if node.status() == GraphStatus::Failed {
        node.note.clone()
    } else {
        node.text.clone().or_else(|| {
            (node.status() == GraphStatus::Done)
                .then(|| node.note.clone())
                .flatten()
        })
    };
    (elapsed_of(node, now).map_or_default(|t| format!(" · {t}")), said)
}

fn wave_summary_node_line(node: &DagNode, run: &DagRun, bound: usize, style: &RlmStyle, now: f64) -> String {
    let head = format!("{} · category:{}", node.name, node.category);
    let (tail, said) = wave_node_tail_and_said(node, run, now);
    let room = bound as isize - 4 - visible_width(&head) as isize - visible_width(&tail) as isize - 3;
    let text = match said {
        Some(s) if room >= 8 => format!(" · {}", cut(&s, room as usize)),
        _ => String::new(),
    };
    let glyph = if node.status() == GraphStatus::Running {
        "▶"
    } else {
        dot(node.status())
    };
    let color = match node.status() {
        GraphStatus::Pending => Fg::Dim,
        GraphStatus::Failed => Fg::Error,
        _ => Fg::Muted,
    };
    cut(
        &format!(
            "  {} {}",
            style.fg(tone(node.status()), glyph),
            style.fg(color, &format!("{head}{text}{tail}"))
        ),
        bound,
    )
}

/// "▶ <title> running wave i/N  d/T done, r running" and one line per node, while a run's children or workflow
/// nodes run or wait; empty otherwise.
pub fn render_wave_summary(runs: &[DagRun], width: usize, style: &RlmStyle, now: f64, max_nodes: usize) -> Vec<String> {
    let bound = width.max(1);
    let Some(run) = runs
        .iter()
        .rev()
        .find(|r| r.nodes.iter().any(|n| summarized(n.kind()) && is_active(n.status())))
    else {
        return Vec::new();
    };
    let c = run_counts(run);
    let state = if c.running > 0 { "running" } else { "waiting" };
    let tail = format!(
        " {state} wave {}/{}  {}/{} done, {} running",
        c.wave, c.waves, c.done, c.total, c.running
    );
    let title_room = (bound as isize - 2 - visible_width(&tail) as isize).max(4) as usize;
    let mut lines = vec![cut(
        &format!(
            "{} {}{}",
            style.fg(Fg::Accent, "▶"),
            style.bold(&style.fg(Fg::Text, &cut(&run.title, title_room))),
            style.fg(Fg::Muted, &tail)
        ),
        bound,
    )];
    let mut nodes: Vec<&DagNode> = run.nodes.iter().collect();
    nodes.sort_by_key(|n| summary_rank(n.status()));
    let max = max_nodes.max(1);
    let shown = if nodes.len() > max {
        &nodes[..max - 1]
    } else {
        &nodes[..]
    };
    for node in shown {
        lines.push(wave_summary_node_line(node, run, bound, style, now));
    }
    if nodes.len() > shown.len() {
        lines.push(cut(
            &style.fg(Fg::Dim, &format!("  +{} more", nodes.len() - shown.len())),
            bound,
        ));
    }
    lines
}
