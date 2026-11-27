use std::collections::HashMap;

use jsrt::number;
use theme::Fg;
use tui::{truncate_to_width, visible_width};

use crate::flat;
use crate::visualizer::{
    CellStatus, RlmFrame, RlmLimits, RlmPool, RlmSnapshot, RlmStyle, RlmUsage, format_duration, render_context_lines,
};

use super::build::{build_rlm_graph, max_defined, sum};
use super::{
    Gauge, GraphKind, GraphNode, GraphProgress, GraphRow, GraphStatus, GraphTotals, RlmViewOptions, RowRenderOptions,
    bar, count_active_tasks, find_node, format_bytes, format_cost, format_tokens, graph_active, graph_totals,
    is_finished, layout_graph, node_elapsed, spinner_glyph, status_color, status_glyph, window_rows,
};

fn meta(node: &GraphNode, now: f64, compact: bool) -> String {
    let mut parts = Vec::new();
    if let Some(t) = node_elapsed(node, now) {
        parts.push(t);
    }
    if !compact && let Some(t) = node.tokens.filter(|t| *t > 0.0) {
        parts.push(format!("{} tok", format_tokens(t)));
    }
    if !compact && let Some(c) = node.cost.filter(|c| *c > 0.0) {
        parts.push(format_cost(c));
    }
    parts.join(" ")
}

fn progress_text(p: &GraphProgress, cells: usize, style: &RlmStyle) -> String {
    let finished = p.done + p.incomplete + p.failed;
    let mut parts = vec![format!(
        "{} {finished}/{}",
        style.fg(
            if p.failed + p.incomplete > 0 {
                Fg::Warning
            } else {
                Fg::Success
            },
            &bar(finished as f64 / p.total.max(1) as f64, cells)
        ),
        p.total
    )];
    if p.running > 0 {
        parts.push(format!("{} running", p.running));
    }
    if p.incomplete > 0 {
        parts.push(style.fg(Fg::Warning, &format!("{} incomplete", p.incomplete)));
    }
    if p.failed > 0 {
        parts.push(style.fg(Fg::Error, &format!("{} failed", p.failed)));
    }
    parts.join(" · ")
}

/// The connector prefix of a row; deep trees compress rails to one column per level on narrow widths.
fn connector(row: &GraphRow, width: usize) -> String {
    if row.depth == 0 {
        return String::new();
    }
    let compress = row.depth * 3 > 12.max(width / 3);
    let (rail, gap) = if compress { ("│", " ") } else { ("│  ", "   ") };
    let tee = match (compress, row.last) {
        (true, true) => "└",
        (true, false) => "├",
        (false, true) => "└─ ",
        (false, false) => "├─ ",
    };
    let mut out: String = row.rails.iter().map(|last| if *last { gap } else { rail }).collect();
    out.push_str(tee);
    out
}

fn row_gutter(o: &RowRenderOptions, selected: bool) -> String {
    if !o.gutter {
        return String::new();
    }
    if selected {
        o.style.fg(Fg::Accent, "▶ ")
    } else {
        "  ".into()
    }
}

fn row_glyph(node: &GraphNode, o: &RowRenderOptions) -> String {
    let color = status_color(node.status);
    if node.kind == GraphKind::Turn {
        let icon = if node.status == GraphStatus::Running {
            &o.spinner
        } else {
            "◆"
        };
        o.style.fg(color, icon)
    } else {
        o.style.fg(color, &status_glyph(node.status, &o.spinner))
    }
}

fn row_note_part(node: &GraphNode, style: &RlmStyle) -> Option<String> {
    let n = node.note.as_deref().filter(|n| !n.is_empty())?;
    let note_color = match node.status {
        GraphStatus::Failed => Fg::Error,
        GraphStatus::Incomplete => Fg::Warning,
        _ => Fg::Muted,
    };
    Some(style.fg(note_color, n))
}

fn row_label(node: &GraphNode, selected: bool, style: &RlmStyle) -> String {
    let label_color = match node.kind {
        GraphKind::Earlier => Fg::Muted,
        GraphKind::Turn | GraphKind::Cell => Fg::ToolTitle,
        _ => Fg::Text,
    };
    let colored = style.fg(label_color, &node.label);
    if selected { style.bold(&colored) } else { colored }
}

fn row_parts(row: &GraphRow, narrow: bool, selected: bool, o: &RowRenderOptions) -> Vec<String> {
    let style = &o.style;
    let node = row.node;
    let label = row_label(node, selected, style);
    let fold = if row.collapsed {
        format!("{} ", style.fg(Fg::Dim, &format!("[+{}]", row.hidden)))
    } else {
        String::new()
    };
    let mut parts = vec![format!("{} {fold}{label}", row_glyph(node, o))];
    if let Some(p) = &node.progress
        && matches!(node.kind, GraphKind::Fanout | GraphKind::Workflow)
    {
        parts.push(progress_text(p, if narrow { 5 } else { 10 }, style));
    }
    if let Some(j) = &node.join {
        parts.push(style.fg(Fg::Accent, j));
    }
    if !narrow && let Some(note) = row_note_part(node, style) {
        parts.push(note);
    }
    parts
}

/// One graph row, fitted to `width`.
pub fn render_graph_row(row: &GraphRow, width: usize, o: &RowRenderOptions) -> String {
    let style = &o.style;
    let narrow = width < 60;
    let selected = o.selected_key.as_deref() == Some(row.node.key.as_str());
    let left = format!(
        "{}{}{}",
        row_gutter(o, selected),
        style.fg(Fg::Dim, &connector(row, width)),
        row_parts(row, narrow, selected, o).join(" ")
    );
    let right = meta(row.node, o.now, narrow);
    if right.is_empty() {
        return truncate_to_width(&left, width, "…", false);
    }
    let right_width = visible_width(&right);
    let room = width as isize - right_width as isize - 1;
    if room < 12 {
        return truncate_to_width(&format!("{left} {}", style.fg(Fg::Dim, &right)), width, "…", false);
    }
    let fitted = truncate_to_width(&left, room as usize, "…", false);
    let pad = (width as isize - visible_width(&fitted) as isize - right_width as isize).max(1) as usize;
    format!("{fitted}{}{}", " ".repeat(pad), style.fg(Fg::Dim, &right))
}

/// Detail lines for an expanded node, indented under it with a dotted rail.
pub fn render_details(
    row: &GraphRow,
    extra: Option<&[(String, String)]>,
    width: usize,
    o: &RowRenderOptions,
    max_lines: usize,
) -> Vec<String> {
    let style = &o.style;
    let indent = format!(
        "{}{}",
        if o.gutter { "  " } else { "" },
        " ".repeat((row.depth * 3 + 2).min(width / 3))
    );
    let entries: Vec<&(String, String)> = row.node.details.iter().chain(extra.unwrap_or(&[]).iter()).collect();
    let mut lines = Vec::new();
    for (label, value) in entries.iter().take(max_lines).map(|e| (&e.0, &e.1)) {
        let color = match label.as_str() {
            "error" | "failed" => Fg::Error,
            "fetch" | "traces" => Fg::Accent,
            _ => Fg::Muted,
        };
        let padded = format!("{label:<9}");
        lines.push(truncate_to_width(
            &format!(
                "{indent}{}{} {}",
                style.fg(Fg::Dim, "┆ "),
                style.fg(Fg::Dim, &padded),
                style.fg(color, value)
            ),
            width,
            "…",
            false,
        ));
    }
    if entries.len() > max_lines {
        lines.push(truncate_to_width(
            &format!(
                "{indent}{}",
                style.fg(Fg::Dim, &format!("┆ … {} more", entries.len() - max_lines))
            ),
            width,
            "…",
            false,
        ));
    }
    lines
}

fn push_usage_gauges(usage: Option<&RlmUsage>, limits: Option<&RlmLimits>, gauges: &mut Vec<Gauge>) {
    if let Some(admitted) = usage.and_then(|u| u.admitted_tasks) {
        let max = limits.and_then(|l| l.max_admitted_tasks);
        gauges.push(Gauge {
            label: "tasks".into(),
            used: admitted,
            limit: max,
            text: match max {
                None => number::to_string(admitted),
                Some(m) => format!("{}/{}", number::to_string(admitted), number::to_string(m)),
            },
        });
    }
    if let (Some(max_wall), Some(remaining)) = (
        limits.and_then(|l| l.max_wall_ms),
        usage.and_then(|u| u.remaining_wall_ms),
    ) {
        let used = (max_wall - remaining).max(0.0);
        gauges.push(Gauge {
            label: "wall".into(),
            used,
            limit: Some(max_wall),
            text: format!("{}/{}", format_duration(used), format_duration(max_wall)),
        });
    }
    let token_limit = limits
        .and_then(|l| l.max_total_tokens)
        .or(usage.and_then(|u| u.max_total_tokens));
    let tokens = usage
        .and_then(|u| {
            u.turn_tokens
                .filter(|t| *t != 0.0)
                .or(u.usage_total_tokens.filter(|t| *t != 0.0))
        })
        .unwrap_or(0.0);
    if tokens > 0.0 || token_limit.is_some() {
        gauges.push(Gauge {
            label: "tokens".into(),
            used: tokens,
            limit: token_limit,
            text: match token_limit {
                None => format_tokens(tokens),
                Some(l) => format!("{}/{}", format_tokens(tokens), format_tokens(l)),
            },
        });
    }
}

fn push_turn_and_cost_gauges(usage: Option<&RlmUsage>, limits: Option<&RlmLimits>, gauges: &mut Vec<Gauge>) {
    let turn_limit = limits
        .and_then(|l| l.max_total_turns)
        .or(usage.and_then(|u| u.max_total_turns));
    if let (Some(limit), Some(turns)) = (turn_limit, usage.and_then(|u| u.turns)) {
        gauges.push(Gauge {
            label: "turns".into(),
            used: turns,
            limit: Some(limit),
            text: format!("{}/{}", number::to_string(turns), number::to_string(limit)),
        });
    }
    let spent = usage.and_then(|u| u.spent_usd.or(u.usage_cost)).unwrap_or(0.0);
    let cost_limit = usage
        .and_then(|u| u.max_cost_usd)
        .or(limits.and_then(|l| l.max_cost_usd));
    if spent > 0.0 || cost_limit.is_some() {
        gauges.push(Gauge {
            label: "cost".into(),
            used: spent,
            limit: cost_limit,
            text: match cost_limit {
                None => format_cost(spent),
                Some(l) => format!("{}/{}", format_cost(spent), format_cost(l)),
            },
        });
    }
}

fn push_frame_budget_gauges(frames: &[RlmFrame], gauges: &mut Vec<Gauge>) {
    let mut order: Vec<String> = Vec::new();
    let mut by_budget: HashMap<String, Vec<&RlmFrame>> = HashMap::new();
    for f in frames {
        let Some(b) = &f.budget else { continue };
        if !by_budget.contains_key(&b.id) {
            order.push(b.id.clone());
        }
        by_budget.entry(b.id.clone()).or_default().push(f);
    }
    let mut candidates: Vec<&Vec<&RlmFrame>> = order.iter().map(|k| &by_budget[k]).collect();
    let running = |fs: &Vec<&RlmFrame>| fs.iter().any(|f| f.status == "running");
    let newest = |fs: &Vec<&RlmFrame>| max_defined(fs.iter().map(|f| f.started_at)).unwrap_or(0.0);
    candidates.sort_by(|l, r| {
        (i32::from(running(r)) - i32::from(running(l)))
            .cmp(&0)
            .then_with(|| newest(r).partial_cmp(&newest(l)).unwrap_or(std::cmp::Ordering::Equal))
    });
    let Some(chosen) = candidates.first() else { return };
    let Some(budget) = chosen[0].budget.as_ref() else {
        return;
    };
    let calls = sum(chosen.iter().map(|f| Some(f.spent_calls))).unwrap_or(0.0);
    let frame_tokens = sum(chosen.iter().map(|f| Some(f.spent_tokens))).unwrap_or(0.0);
    gauges.push(Gauge {
        label: "calls".into(),
        used: calls,
        limit: budget.calls,
        text: match budget.calls {
            None => number::to_string(calls),
            Some(c) => format!("{}/{}", number::to_string(calls), number::to_string(c)),
        },
    });
    if let Some(t) = budget.tokens {
        gauges.push(Gauge {
            label: "frame tok".into(),
            used: frame_tokens,
            limit: Some(t),
            text: format!("{}/{}", format_tokens(frame_tokens), format_tokens(t)),
        });
    }
    gauges.push(Gauge {
        label: "depth".into(),
        used: budget.depth,
        limit: None,
        text: number::to_string(budget.depth),
    });
}

pub fn budget_gauges(snapshot: &RlmSnapshot, _graph: &GraphNode) -> Vec<Gauge> {
    let mut gauges = Vec::new();
    let usage = snapshot.usage.as_ref();
    let limits = snapshot.limits.as_ref();
    push_usage_gauges(usage, limits, &mut gauges);
    push_turn_and_cost_gauges(usage, limits, &mut gauges);
    push_frame_budget_gauges(&snapshot.frames, &mut gauges);
    gauges
}

fn gauge_color(fraction: f64) -> Fg {
    if fraction >= 0.85 {
        Fg::Error
    } else if fraction >= 0.6 {
        Fg::Warning
    } else {
        Fg::Success
    }
}

fn render_single_gauge(g: &Gauge, cells: usize, style: &RlmStyle) -> String {
    match g.limit {
        Some(limit) if limit > 0.0 => {
            let fraction = g.used / limit;
            format!(
                "{} {} {}",
                style.fg(Fg::Dim, &g.label),
                style.fg(gauge_color(fraction), &bar(fraction, cells)),
                style.fg(Fg::Text, &g.text)
            )
        }
        _ => format!("{} {}", style.fg(Fg::Dim, &g.label), style.fg(Fg::Text, &g.text)),
    }
}

/// Gauges packed into at most `max_lines` lines of `width`.
pub fn render_gauges(gauges: &[Gauge], width: usize, style: &RlmStyle, max_lines: usize) -> Vec<String> {
    let cells = if width < 50 { 4 } else { 6 };
    let mut lines = Vec::new();
    let mut current = String::new();
    for item in gauges.iter().map(|g| render_single_gauge(g, cells, style)) {
        let candidate = if current.is_empty() {
            item.clone()
        } else {
            format!("{current}  {item}")
        };
        if visible_width(&candidate) <= width || current.is_empty() {
            current = candidate;
        } else {
            lines.push(std::mem::take(&mut current));
            if lines.len() >= max_lines {
                break;
            }
            current = item;
        }
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }
    lines.iter().map(|l| truncate_to_width(l, width, "…", false)).collect()
}

fn format_guard_ms(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{}ms", number::to_string(ms.round()))
    } else {
        format!("{}s", number::to_fixed(ms / 1000.0, 1))
    }
}

fn kernel_pool_head(pool: &RlmPool, width: usize, style: &RlmStyle) -> String {
    if pool.max_live > 24.0 || width < 50 {
        return format!(
            "{} {}/{}",
            style.fg(Fg::Dim, "kernels"),
            number::to_string(pool.live),
            number::to_string(pool.max_live)
        );
    }
    let cells: String = pool
        .lanes
        .iter()
        .map(|l| {
            if l.running > 0.0 {
                style.fg(Fg::Accent, "■")
            } else if !l.pinned_by.is_empty() {
                style.fg(Fg::Warning, "◆")
            } else {
                style.fg(Fg::Muted, "□")
            }
        })
        .collect();
    let free = (pool.max_live as isize - pool.lanes.len() as isize).max(0) as usize;
    format!(
        "{} {cells}{} {}/{}",
        style.fg(Fg::Dim, "kernels"),
        style.fg(Fg::Dim, &"·".repeat(free)),
        number::to_string(pool.live),
        number::to_string(pool.max_live)
    )
}

fn append_pool_guards(pool: &RlmPool, style: &RlmStyle, parts: &mut Vec<String>) {
    for g in pool.guards.iter().flatten() {
        if g.checks == 0.0 && g.after_checks == 0.0 {
            continue;
        }
        let blocked = if g.blocked > 0.0 {
            style.fg(Fg::Warning, &format!(" {} blocked", number::to_string(g.blocked)))
        } else {
            String::new()
        };
        parts.push(format!(
            "{} {} turn/{}{blocked}",
            style.fg(Fg::Dim, &g.name.to_lowercase()),
            format_guard_ms(g.turn_ms),
            format_guard_ms(g.total_ms)
        ));
    }
}

/// `kernels ■■◆□······ 3/16 · 1 busy · 1 pinned · 2 evicted · 212 MiB · loki 85ms turn/1.4s`.
pub fn render_kernel_strip(snapshot: &RlmSnapshot, width: usize, style: &RlmStyle) -> Option<String> {
    let pool = snapshot.pool.as_ref()?;
    let busy = pool.lanes.iter().filter(|l| l.running > 0.0).count();
    let pinned = pool.lanes.iter().filter(|l| !l.pinned_by.is_empty()).count();
    let mut parts = vec![
        kernel_pool_head(pool, width, style),
        format!("{busy} busy"),
        format!("{pinned} pinned"),
    ];
    if let Some(e) = pool.evictions {
        parts.push(format!("{} evicted", number::to_string(e)));
    }
    if let Some(m) = pool.memory_bytes {
        let cap = match pool.memory_cap_bytes {
            Some(c) if width >= 100 => format!(" (cap {}/kernel)", format_bytes(c)),
            _ => String::new(),
        };
        parts.push(format!("mem {}{cap}", format_bytes(m)));
    }
    append_pool_guards(pool, style, &mut parts);
    Some(truncate_to_width(
        &parts.join(&style.fg(Fg::Dim, " · ")),
        width,
        "…",
        false,
    ))
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

fn footer_cell_part(graph: &GraphNode, totals: &GraphTotals, now: f64) -> Option<String> {
    let cell = totals.cell_key.as_deref().and_then(|k| find_node(graph, k))?;
    let index = graph
        .children
        .iter()
        .filter(|c| c.kind == GraphKind::Cell)
        .position(|c| c.key == cell.key)
        .map_or(0, |i| i + 1);
    let suffix = match (cell.status, node_elapsed(cell, now)) {
        (GraphStatus::Running, Some(t)) => format!(" {t}"),
        _ => String::new(),
    };
    Some(format!("cell {index}{suffix}"))
}

fn footer_fanout_part(graph: &GraphNode, totals: &GraphTotals) -> Option<String> {
    let fanouts: Vec<&GraphNode> = totals.fanout_keys.iter().filter_map(|k| find_node(graph, k)).collect();
    let fanout = fanouts
        .iter()
        .find(|n| n.status == GraphStatus::Running)
        .or(fanouts.last())?;
    let p = fanout.progress?;
    let finished = p.done + p.incomplete + p.failed;
    Some(format!(
        "map {} {finished}/{}",
        bar(finished as f64 / p.total.max(1) as f64, 5),
        p.total
    ))
}

fn append_footer_counts(graph: &GraphNode, totals: &GraphTotals, style: &RlmStyle, parts: &mut Vec<String>) {
    if totals.tasks > 0 {
        let active = count_active_tasks(graph);
        let suffix = if active > 0 {
            format!(" ({active} active)")
        } else {
            String::new()
        };
        parts.push(format!("{}{suffix}", plural(totals.tasks, "task")));
    }
    if totals.jobs_running > 0 {
        parts.push(plural(totals.jobs_running, "job"));
    }
    if totals.tools_running > 0 {
        parts.push(plural(totals.tools_running, "tool call"));
    }
    if totals.failed > 0 {
        parts.push(style.fg(Fg::Error, &format!("{} failed", totals.failed)));
    }
    if let Some(c) = graph.cost.filter(|c| *c > 0.0) {
        parts.push(format_cost(c));
    }
}

/// Level (a): one line while the panel is hidden; `None` when nothing runs.
pub fn render_rlm_footer(snapshot: &RlmSnapshot, width: usize, o: &RlmViewOptions) -> Option<String> {
    let graph = build_rlm_graph(snapshot);
    let root_running = snapshot
        .root_cell
        .as_ref()
        .is_some_and(|c| c.status == CellStatus::Running);
    if !graph_active(&graph) && !root_running {
        return None;
    }
    let style = &o.style;
    let spinner = spinner_glyph(o.spinner_frame);
    let totals = graph_totals(&graph);
    let time = node_elapsed(&graph, snapshot.now);
    let mut parts = vec![format!("turn{}", time.map_or_default(|t| format!(" {t}")))];
    parts.extend(footer_cell_part(&graph, &totals, snapshot.now));
    parts.extend(footer_fanout_part(&graph, &totals));
    append_footer_counts(&graph, &totals, style, &mut parts);
    let summary = format!(
        "{} {}",
        style.fg(Fg::Accent, &format!("◆ rlm {spinner}")),
        style.fg(Fg::Muted, &parts.join(" · "))
    );
    let hint = o
        .focus_key
        .as_ref()
        .map_or_default(|k| style.fg(Fg::Dim, &format!(" · {k} graph")));
    let bound = width.max(1);
    if !hint.is_empty() && visible_width(&summary) + visible_width(&hint) <= bound {
        return Some(format!("{summary}{hint}"));
    }
    Some(truncate_to_width(&summary, bound, "…", false))
}

fn header_line(graph: &GraphNode, snapshot: &RlmSnapshot, style: &RlmStyle, spinner: &str, suffix: &str) -> String {
    let totals = graph_totals(graph);
    let state = if graph_active(graph) {
        style.fg(Fg::Accent, &format!("{spinner} running"))
    } else if graph.status == GraphStatus::Failed {
        style.fg(Fg::Error, "✗ failed")
    } else if graph.children.is_empty() {
        style.fg(Fg::Dim, "idle")
    } else {
        style.fg(Fg::Success, "✓ done")
    };
    let mut parts = vec![format!("{} nodes", totals.nodes - 1)];
    if totals.running > 0 {
        parts.push(format!("{} active", totals.running));
    }
    if let Some(e) = &snapshot.error {
        parts.push(style.fg(Fg::Error, &format!("inspection failed: {}", flat(e, 80))));
    }
    format!(
        "{} {state} {}{suffix}",
        style.bold(&style.fg(Fg::Accent, "RLM")),
        style.fg(Fg::Muted, &parts.join(" · "))
    )
}

/// Level (b): the docked panel.
pub fn render_rlm_dock(snapshot: &RlmSnapshot, width: usize, o: &RlmViewOptions, max_rows: usize) -> Vec<String> {
    let style = &o.style;
    let spinner = spinner_glyph(o.spinner_frame);
    let bound = width.max(1);
    let graph = build_rlm_graph(snapshot);
    let hint = o
        .focus_key
        .as_ref()
        .map_or_default(|k| style.fg(Fg::Dim, &format!("  {k} focus")));
    let mut lines = vec![truncate_to_width(
        &header_line(&graph, snapshot, style, spinner, &hint),
        bound,
        "…",
        false,
    )];
    lines.extend(render_gauges(&budget_gauges(snapshot, &graph), bound, style, 2));
    let all = layout_graph(&graph, &o.collapse);
    let row_options = RowRenderOptions::new(style.clone(), snapshot.now, spinner);
    lines.push(render_graph_row(&all[0], bound, &row_options));
    let rows = &all[1..];
    let max = 2.max(max_rows.saturating_sub(1));
    let focus = rows
        .iter()
        .rposition(|r| r.node.status == GraphStatus::Running)
        .unwrap_or(rows.len().saturating_sub(1));
    let (start, end) = if rows.is_empty() {
        (0, 0)
    } else {
        window_rows(rows.len(), max, focus)
    };
    if start > 0 {
        lines.push(style.fg(
            Fg::Dim,
            &truncate_to_width(&format!("   ↑ {start} more"), bound, "…", false),
        ));
    }
    for row in &rows[start..end] {
        lines.push(render_graph_row(row, bound, &row_options));
    }
    if end < rows.len() {
        let below = &rows[end..];
        let active = below.iter().filter(|r| !is_finished(r.node.status)).count();
        lines.push(style.fg(
            Fg::Dim,
            &truncate_to_width(
                &format!(
                    "  ↓ {} more{}",
                    below.len(),
                    if active > 0 {
                        format!(" ({active} active)")
                    } else {
                        String::new()
                    }
                ),
                bound,
                "…",
                false,
            ),
        ));
    }
    if let Some(strip) = render_kernel_strip(snapshot, bound, style) {
        lines.push(strip);
    }
    lines.extend(render_context_lines(snapshot.context.as_ref(), style));
    lines.iter().map(|l| truncate_to_width(l, bound, "…", false)).collect()
}
